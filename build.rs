fn main() {
    glib_build_tools::compile_resources(&["data"], "data/koshi.gresource.xml", "koshi.gresource");
}
