fn main() {
    for address in config::first_run::lan_addresses() {
        println!("{address}");
    }
}
