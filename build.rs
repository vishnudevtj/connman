fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .type_attribute(
            ".connman.AddTLSListenerRequest",
            "#[derive(serde::Serialize, serde::Deserialize)]",
        )
        .type_attribute(
            ".connman.RegisterImageRequest",
            "#[derive(serde::Serialize, serde::Deserialize)]",
        )
        .type_attribute(
            ".connman.AddProxyRequest",
            "#[derive(serde::Serialize, serde::Deserialize)]",
        )
        .type_attribute(
            ".connman.RemoveProxyRequest",
            "#[derive(serde::Serialize, serde::Deserialize)]",
        )
        .compile(&["proto/connman.proto"], &["proto"])?;
    Ok(())
}
