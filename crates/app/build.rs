fn main() {
    println!("cargo:rerun-if-changed=kuery.rc");
    println!("cargo:rerun-if-changed=assets/branding/kuery.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // GPUI loads the application's icon from resource ID 1.
        embed_resource::compile("kuery.rc", embed_resource::NONE)
            .manifest_required()
            .expect("embed the Kuery application icon");
    }
}
