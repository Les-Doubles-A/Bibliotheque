fn main() {
    println!("cargo:rerun-if-changed=bibliotheque.ico");
    println!("cargo:rerun-if-changed=app.rc");
    let _ = embed_resource::compile("app.rc", embed_resource::NONE);
}
