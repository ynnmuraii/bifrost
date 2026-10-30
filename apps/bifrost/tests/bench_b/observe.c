/* Fixture only: gcc -shared -fPIC -O2 -o libbf_bench_observer.so observe.c -pthread
 * Records contain synthetic dry IPC only; never preload into a native session. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define MAX_FRAME 65536
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int enabled, output = -1;
static _Atomic int selected = -1;
struct stream { unsigned char data[MAX_FRAME + 4]; size_t used, need; int broken; };
static struct stream streams[2];

static void le(unsigned char *p, uint64_t n, unsigned width) {
    for (unsigned i = 0; i < width; ++i) { p[i] = n & 255; n >>= 8; }
}
static uint64_t now_ns(void) {
    struct timespec t;
    if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &t)) return 0;
    return (uint64_t)t.tv_sec * 1000000000ULL + t.tv_nsec;
}
static int varint(const unsigned char *p, size_t n, size_t *at, uint64_t *v) {
    *v = 0;
    for (unsigned shift = 0; shift < 70 && *at < n; shift += 7) {
        unsigned c = p[(*at)++];
        if (shift == 63 && c > 1) return 0;
        *v |= (uint64_t)(c & 127) << shift;
        if (!(c & 128)) return 1;
    }
    return 0;
}
static int allowed(unsigned tag) {
    return (tag >= 11 && tag <= 16) || tag == 40 || tag == 41 || tag == 42 ||
           (tag >= 44 && tag <= 46) || tag == 51;
}
static void raw_write(const void *p, size_t n) {
    while (n && output >= 0) {
        ssize_t k = syscall(SYS_write, output, p, n);
        if (k < 0 && errno == EINTR) continue;
        if (k <= 0) { syscall(SYS_close, output); output = -1; break; }
        p = (const unsigned char *)p + k; n -= (size_t)k;
    }
}
static void record(unsigned dir, unsigned tag, uint64_t seq,
                   const unsigned char *p, size_t n, uint64_t stamp) {
    unsigned char h[32] = {'B','B','O','1',0};
    h[4] = dir; h[5] = tag;
    le(h + 8, stamp, 8); le(h + 16, seq, 8);
    le(h + 24, (uint32_t)getpid(), 4); le(h + 28, n, 4);
    raw_write(h, sizeof h); raw_write(p, n);
}
static void frame(unsigned dir, const unsigned char *p, size_t n, uint64_t stamp) {
    size_t at = 0, body_at = 0, body_n = 0;
    uint64_t seq = 0; unsigned tag = 0;
    while (at < n) {
        uint64_t key, v;
        if (!varint(p, n, &at, &key) || !key) return;
        unsigned field = key >> 3, wire = key & 7;
        if (wire == 0) {
            if (!varint(p, n, &at, &v)) return;
            if (field == 3) seq = v;
        } else if (wire == 2) {
            if (!varint(p, n, &at, &v) || v > n - at) return;
            if (field >= 10) { tag = field; body_at = at; body_n = (size_t)v; }
            at += (size_t)v;
        } else if (wire == 1 || wire == 5) {
            size_t width = wire == 1 ? 8 : 4;
            if (width > n - at) return;
            at += width;
        } else return;
    }
    /* Error detail is deliberately discarded, as are all non-allowlisted messages. */
    if (allowed(tag)) record(dir, tag, seq, p + body_at, tag == 40 || tag == 51 ? 0 : body_n, stamp);
}
static void feed(unsigned dir, const void *bytes, size_t n, uint64_t stamp) {
    struct stream *s = &streams[dir];
    const unsigned char *p = bytes;
    while (n && !s->broken) {
        size_t goal = s->used < 4 ? 4 : s->need;
        size_t k = goal - s->used; if (k > n) k = n;
        memcpy(s->data + s->used, p, k); s->used += k; p += k; n -= k;
        if (s->used == 4 && !s->need) {
            size_t size = ((size_t)s->data[0] << 24) | ((size_t)s->data[1] << 16) |
                          ((size_t)s->data[2] << 8) | s->data[3];
            if (!size || size > MAX_FRAME) {
                s->broken = 1; record(dir, 0, 0, NULL, 0, stamp); break;
            }
            s->need = size + 4;
        }
        if (s->need && s->used == s->need) {
            frame(dir, s->data + 4, s->need - 4, stamp);
            /* Do not retain an Attach/token or clipboard in an idle parsing buffer. */
            memset(s->data, 0, s->used); s->used = s->need = 0;
        }
    }
}
static void vectors(unsigned dir, const struct iovec *v, size_t count, size_t n, uint64_t stamp) {
    for (size_t i = 0; i < count && n; ++i) {
        size_t k = v[i].iov_len < n ? v[i].iov_len : n;
        feed(dir, v[i].iov_base, k, stamp); n -= k;
    }
}
__attribute__((constructor)) static void initialize(void) {
    int saved = errno;
    char args[16384]; int fd = syscall(SYS_openat, AT_FDCWD, "/proc/self/cmdline", O_RDONLY | O_CLOEXEC, 0);
    ssize_t n = fd < 0 ? -1 : syscall(SYS_read, fd, args, sizeof args - 1);
    if (fd >= 0) syscall(SYS_close, fd);
    int agent = 0, dry = 0;
    if (n > 0) {
        args[n] = 0;
        for (size_t at = 0; at < (size_t)n;) {
            size_t len = strnlen(args + at, (size_t)n - at);
            if (!strcmp(args + at, "agent")) agent = 1;
            if (!strcmp(args + at, "--dry-inject")) dry = 1;
            at += len + 1;
        }
    }
    const char *path = getenv("BF_BENCH_OBSERVE");
    if (agent && dry && path && *path) {
        output = syscall(SYS_openat, AT_FDCWD, path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC | O_NOFOLLOW, 0600);
        enabled = output >= 0;
    }
    errno = saved;
}
int connect(int fd, const struct sockaddr *addr, socklen_t len) {
    int entry = errno;
    int candidate = 0;
    if (enabled && addr && len > sizeof(sa_family_t) && addr->sa_family == AF_UNIX) {
        const struct sockaddr_un *u = (const struct sockaddr_un *)addr;
        size_t cap = len - sizeof(sa_family_t); if (cap > sizeof u->sun_path) cap = sizeof u->sun_path;
        char path[sizeof u->sun_path + 1]; memcpy(path, u->sun_path, cap); path[cap] = 0;
        const char *base = strrchr(path, '/'); base = base ? base + 1 : path;
        size_t n = strlen(base);
        candidate = n > 11 && !strncmp(base, "agent-", 6) && !strcmp(base + n - 5, ".sock");
    }
    pthread_mutex_lock(&lock); errno = entry;
    int r = syscall(SYS_connect, fd, addr, len), saved = errno;
    if (candidate && (r == 0 || saved == EINPROGRESS)) {
        selected = fd; memset(streams, 0, sizeof streams);
        record(0, 1, 0, NULL, 0, now_ns());
    }
    pthread_mutex_unlock(&lock); errno = saved; return r;
}
/* The mutex covers each selected syscall and parsing so concurrent Tokio tasks cannot
 * reorder frames. Unselected descriptors use the same original Linux syscall. */
#define BEGIN int entry = errno; int guarded = fd == selected; if (guarded) pthread_mutex_lock(&lock); errno = entry
#define END int saved = errno; uint64_t stamp = guarded ? now_ns() : 0
#define RETURN if (guarded) pthread_mutex_unlock(&lock); errno = saved; return r
ssize_t read(int fd, void *p, size_t n) {
    BEGIN; ssize_t r = syscall(SYS_read, fd, p, n); END;
    if (fd == selected && r > 0) feed(0, p, r, stamp); RETURN;
}
ssize_t write(int fd, const void *p, size_t n) {
    BEGIN; ssize_t r = syscall(SYS_write, fd, p, n); END;
    if (fd == selected && r > 0) feed(1, p, r, stamp); RETURN;
}
ssize_t recv(int fd, void *p, size_t n, int flags) {
    BEGIN; ssize_t r = syscall(SYS_recvfrom, fd, p, n, flags, NULL, NULL); END;
    if (fd == selected && r > 0 && !(flags & MSG_PEEK)) feed(0, p, (size_t)r < n ? (size_t)r : n, stamp); RETURN;
}
ssize_t send(int fd, const void *p, size_t n, int flags) {
    BEGIN; ssize_t r = syscall(SYS_sendto, fd, p, n, flags, NULL, 0); END;
    if (fd == selected && r > 0) feed(1, p, r, stamp); RETURN;
}
ssize_t writev(int fd, const struct iovec *v, int count) {
    BEGIN; ssize_t r = syscall(SYS_writev, fd, v, count); END;
    if (fd == selected && r > 0) vectors(1, v, count, r, stamp); RETURN;
}
ssize_t readv(int fd, const struct iovec *v, int count) {
    BEGIN; ssize_t r = syscall(SYS_readv, fd, v, count); END;
    if (fd == selected && r > 0) vectors(0, v, count, r, stamp); RETURN;
}
ssize_t sendmsg(int fd, const struct msghdr *msg, int flags) {
    BEGIN; ssize_t r = syscall(SYS_sendmsg, fd, msg, flags); END;
    if (fd == selected && r > 0) vectors(1, msg->msg_iov, msg->msg_iovlen, r, stamp); RETURN;
}
ssize_t recvmsg(int fd, struct msghdr *msg, int flags) {
    BEGIN; ssize_t r = syscall(SYS_recvmsg, fd, msg, flags); END;
    if (fd == selected && r > 0 && !(flags & MSG_PEEK)) vectors(0, msg->msg_iov, msg->msg_iovlen, r, stamp); RETURN;
}
int close(int fd) {
    BEGIN; int r = syscall(SYS_close, fd); int saved = errno;
    if (fd == selected && r == 0) {
        if (streams[0].used || streams[1].used) record(0, 0, 0, NULL, 0, now_ns());
        selected = -1;
    }
    RETURN;
}
