//! What reads senders' mail is built the cautious way by
//! `.cargo/config.toml`'s flags (src/mail.rs, `PARSER`), which `cargo
//! install` from crates.io doesn't read and a RUSTFLAGS replaces. A build
//! without them says so (where cargo shows it: a crates.io build is told
//! at run time, `mail::HARDENED`), and one for CI or a release
//! (TM_HARDENED) fails.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=TM_HARDENED");
    println!("cargo::rustc-check-cfg=cfg(mail_parser_checked_str)");
    println!("cargo::rustc-check-cfg=cfg(encodify_scalar)");
    let set = |name: &str| std::env::var_os(format!("CARGO_CFG_{name}")).is_some();
    if set("MAIL_PARSER_CHECKED_STR") && set("ENCODIFY_SCALAR") {
        return;
    }
    let without = "built without RUSTFLAGS=\"--cfg mail_parser_checked_str --cfg encodify_scalar\": \
        mail-parser then trusts its own SIMD check of the strings it makes, and encodify \
        decodes with its unsafe SIMD kernels (README: build it from source)";
    if std::env::var_os("TM_HARDENED").is_some() {
        panic!("{without}");
    }
    println!("cargo::warning={without}");
}
