fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "{}",
        serde_json::to_string_pretty(
            &amaterasu_rtx_hdr::settings_descriptor::installation_metadata()
        )?
    );
    Ok(())
}
