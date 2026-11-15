//! Embeds the Windows executable's icon (`windows/lightfast.rc`).

fn main() {
    println!("cargo:rerun-if-changed=windows/lightfast.rc");
    println!("cargo:rerun-if-changed=windows/lightfast.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        embed_resource::compile_for("windows/lightfast.rc", ["lightfast"], embed_resource::NONE)
            .manifest_required()
            .expect("the icon resource should compile");
    }
}
