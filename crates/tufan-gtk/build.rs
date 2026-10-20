fn main() {
    // No-op unless targeting Windows; there a missing resource compiler
    // must fail the build rather than ship an exe without its icon.
    embed_resource::compile("tufan.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
