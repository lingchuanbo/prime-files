//! Print the `quicklaunch.toml` JSON Schema to stdout.
//!
//! This emits the exact bytes committed to `docs/quicklaunch.schema.json`, but
//! builds only `nohrs-core`, so it runs on Linux where the gpui-backed `nohrs`
//! binary does not link. CI uses it to keep the committed schema in sync:
//!
//! ```bash
//! cargo run -p nohrs-core --example quicklaunch_schema > docs/quicklaunch.schema.json
//! ```

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "{}",
        nohrs_core::config::quick_launch::json_schema_string()?
    );
    Ok(())
}
