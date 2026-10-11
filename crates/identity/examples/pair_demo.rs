//! Pairing smoke: two in-process endpoints over loopback, then an unknown peer refused.
//!
//! What it proves (KVM-030 DoD: «Два fingerprints, unknown peers inactive, revoke»):
//!
//! 1. both nodes derive the same 6-digit code from their own TLS exporter
//!    (`TLS-Exporter("EXPORTER-lan-workspace-pair", attempt_id, 4 байта) mod 10⁶`), and
//!    both print the full fingerprints;
//! 2. `PairDecision` round-trips and each side writes a `TRUSTED_PEER` record;
//! 3. a **third, unknown** identity is refused on the input ALPN, because the pinned
//!    verifier only accepts an active trust record — the handshake dies before any
//!    `Hello`;
//! 4. revoking the paired peer makes it fail the same way, and the trust file shows the
//!    `revoked_at` stamp.
//!
//! Run with: `cargo run -p identity --example pair_demo`

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use identity::{
    accept_one, bind, dial, now_unix_ms, open_window, pin_error, server_name,
    trusted_client_config, trusted_server_config, IdentityPaths, NodeIdentity, TrustPolicy,
    TrustStore,
};
use tokio::net::TcpListener;
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!("lan-workspace-pair-demo-{}", std::process::id()));
    let dir_a = root.join("node-a");
    let dir_b = root.join("node-b");
    let dir_c = root.join("node-c");

    // --- identities ---------------------------------------------------------------
    let (identity_a, load_a) = NodeIdentity::load_or_create(&IdentityPaths::rooted_at(&dir_a))?;
    let (identity_b, load_b) = NodeIdentity::load_or_create(&IdentityPaths::rooted_at(&dir_b))?;
    let (identity_c, _) = NodeIdentity::load_or_create(&IdentityPaths::rooted_at(&dir_c))?;
    println!("== identities ==");
    println!(
        "A (window opener): {} ({:?}) fingerprint {}",
        identity_a.node_id(),
        load_a,
        identity_a.fingerprint_grouped()
    );
    println!(
        "B (dialer):        {} ({:?}) fingerprint {}",
        identity_b.node_id(),
        load_b,
        identity_b.fingerprint_grouped()
    );
    println!(
        "C (unknown):       {} fingerprint {}",
        identity_c.node_id(),
        identity_c.fingerprint_grouped()
    );

    // --- pairing window ----------------------------------------------------------
    let listener = bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
    let address = listener.local_addr()?;
    let mut window = open_window(0)?;
    println!(
        "\n== pairing: window open on {address}, attempt_id {} ==",
        hex(&window.attempt_id()[..4])
    );

    let server = accept_one(&listener, &identity_a, &mut window, 0);
    let client = dial(address, &identity_b, Duration::from_secs(120));
    let (driver_a, driver_b) = tokio::try_join!(server, client)?;

    println!("SAS on A: {}", driver_a.sas());
    println!("SAS on B: {}", driver_b.sas());
    assert_eq!(
        driver_a.sas(),
        driver_b.sas(),
        "both sides must derive the same comparison code from the same TLS session"
    );
    assert_eq!(driver_a.peer_node_id(), &identity_b.node_id());
    assert_eq!(driver_b.peer_node_id(), &identity_a.node_id());
    println!("both sides derived the same code: OK");

    // Both users compare the codes and confirm.
    let (paired_a, paired_b) = tokio::try_join!(driver_a.approve(), driver_b.approve())?;
    println!("\n== committed ==");
    println!(
        "A paired with {} ({})",
        paired_a.node_id,
        identity::fingerprint_grouped(&paired_a.node_id)
    );
    println!(
        "B paired with {} ({})",
        paired_b.node_id,
        identity::fingerprint_grouped(&paired_b.node_id)
    );

    let mut trust_a = TrustStore::new();
    trust_a.trust(paired_a.trusted_peer(now_unix_ms(), Some(address.to_string())));
    trust_a.save(&IdentityPaths::rooted_at(&dir_a).trust_file)?;
    let mut trust_b = TrustStore::new();
    trust_b.trust(paired_b.trusted_peer(now_unix_ms(), Some(address.to_string())));
    trust_b.save(&IdentityPaths::rooted_at(&dir_b).trust_file)?;
    println!("\n== trust.toml on A ==\n{}", trust_a.to_toml()?);
    assert!(
        trust_a.is_trusted(&identity_b.node_id()),
        "B is now trusted on A"
    );
    assert!(
        !trust_a.is_trusted(&identity_c.node_id()),
        "C is not trusted on A"
    );

    // --- unknown peer on the *input* channel -------------------------------------
    println!("== unknown peer on the input channel (ALPN lan-workspace/input/1) ==");
    // C is made to pin A, so only A's own trust store can refuse the handshake.
    let mut trust_c = TrustStore::new();
    trust_c.trust(identity::TrustedPeer::paired(
        identity_a.node_id(),
        "node-a",
        None,
        now_unix_ms(),
    ));
    let (input_listener, acceptor) = secure_listener(&identity_a, &trust_a).await?;
    let input_address = input_listener.local_addr()?;
    let unknown_refused = unknown_peer_is_refused(
        input_listener,
        acceptor,
        input_address,
        &identity_c,
        &trust_c,
    )
    .await;
    println!("unknown peer refused on the input channel: {unknown_refused}");
    assert!(
        unknown_refused,
        "an identity without a trust record must not complete the input handshake"
    );

    // --- revoke ------------------------------------------------------------------
    println!("\n== revoke B and retry the input channel ==");
    let revoked_at = now_unix_ms();
    assert!(trust_a.revoke(&identity_b.node_id(), revoked_at));
    trust_a.save(&IdentityPaths::rooted_at(&dir_a).trust_file)?;
    println!(
        "revoked_at_unix_ms = {revoked_at}, pinned peers now: {}",
        trust_a.pinned().len()
    );
    assert!(matches!(
        TrustStore::load(&IdentityPaths::rooted_at(&dir_a).trust_file)?,
        identity::atomic::LoadResult::Loaded(_)
    ));
    let (input_listener, acceptor) = secure_listener(&identity_a, &trust_a).await?;
    let input_address = input_listener.local_addr()?;
    let paired_but_revoked_refused = unknown_peer_is_refused(
        input_listener,
        acceptor,
        input_address,
        &identity_b,
        &trust_b,
    )
    .await;
    println!("revoked peer refused on the input channel: {paired_but_revoked_refused}");
    assert!(
        paired_but_revoked_refused,
        "«TRUSTED_PEER с revoked_at не может открыть сеанс»"
    );

    std::fs::remove_dir_all(&root).ok();
    println!("\nPAIR DEMO OK");
    Ok(())
}

/// A listener whose verifier accepts only the pinned peers in `trust`.
async fn secure_listener(
    identity: &NodeIdentity,
    trust: &TrustStore,
) -> Result<(TcpListener, TlsAcceptor), Box<dyn std::error::Error>> {
    // The configuration is built here (and checked below) so the demo also exercises the
    // policy/ALPN invariant: `server_config` refuses a pairing policy on
    // `lan-workspace/input/1`, which is what keeps unknown peers off the runtime channels.
    let config = trusted_server_config(identity, trust)?;
    assert_eq!(config.alpn_protocols.len(), identity::LINK_ALPN.len());
    assert!(matches!(
        identity::server_config(
            identity,
            TrustPolicy::Pairing,
            identity::LINK_ALPN.as_slice()
        ),
        Err(identity::IdentityError::Tls(_))
    ));
    let listener = bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
    Ok((listener, TlsAcceptor::from(std::sync::Arc::new(config))))
}

/// Dials the input ALPN as `dialer` and reports whether the handshake was refused.
async fn unknown_peer_is_refused(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    address: SocketAddr,
    dialer: &NodeIdentity,
    dialer_trust: &TrustStore,
) -> bool {
    // The dialer *does* pin the server, so a refusal can only come from the server's own
    // verifier: that is the "unknown peers are inactive" property, tested one-directionally.
    let client_config = match trusted_client_config(dialer, dialer_trust) {
        Ok(config) => config,
        Err(error) => {
            println!("  client config error: {error}");
            return false;
        }
    };
    let connector = TlsConnector::from(std::sync::Arc::new(client_config));
    let name = server_name(address.ip());

    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await?;
        acceptor.accept(tcp).await
    });
    let client = async {
        let tcp = tokio::net::TcpStream::connect(address).await?;
        connector.connect(name, tcp).await
    };
    let (server_result, client_result) = tokio::join!(server, client);
    let server_error = server_result.ok().and_then(|result| result.err());
    if let Some(error) = &server_error {
        match rustls_error_of(error).and_then(pin_error) {
            Some(refusal) => println!("  server refused: {refusal}"),
            None => println!("  server error: {error}"),
        }
    }
    if let Err(error) = &client_result {
        println!(
            "  client saw: {}",
            rustls_error_of(error).map_or_else(|| error.to_string(), |error| error.to_string())
        );
    }
    server_error
        .as_ref()
        .is_some_and(|error| rustls_error_of(error).and_then(pin_error).is_some())
}

/// tokio-rustls reports handshake failures as `io::Error` with the rustls error as its
/// source; this digs it out so the demo can tell a pin refusal from a socket problem.
fn rustls_error_of(error: &std::io::Error) -> Option<&rustls::Error> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
