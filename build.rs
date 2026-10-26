//! What reads senders' mail is built the cautious way, by flags
//! `.cargo/config.toml` gives (src/mail.rs, `PARSER`). `cargo install`
//! from crates.io doesn't read that file, and a RUSTFLAGS replaces it, so
//! a build without them stops here and says how to make one: cargo hides
//! a crates.io package's warnings, not why its build failed.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    // A RUSTFLAGS set later replaces the flags: check again.
    println!("cargo::rerun-if-env-changed=RUSTFLAGS");
    println!("cargo::rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    let set = |name: &str| std::env::var_os(format!("CARGO_CFG_{name}")).is_some();
    if set("MAIL_PARSER_CHECKED_STR") && set("ENCODIFY_SCALAR") {
        return;
    }
    eprintln!(
        "tuimail is built so that what reads your mail checks every string it makes \
        and decodes without unsafe SIMD code, by two flags. Build it with them:\n\n    \
        RUSTFLAGS=\"--cfg mail_parser_checked_str --cfg encodify_scalar\" \
        cargo install tuimail --locked\n\n\
        (A RUSTFLAGS of your own replaces the repository's .cargo/config.toml: add them to it.)"
    );
    std::process::exit(1);
}
