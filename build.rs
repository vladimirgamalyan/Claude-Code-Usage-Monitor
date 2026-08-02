use winres::WindowsResource;

fn main() {
    // Embed the application icon into the executable. The version fields winres
    // fills in by default are cleared: the app does not track its own version.
    let mut res = WindowsResource::new();
    res.set_icon("src/icons/icon.ico")
        .set("FileVersion", "")
        .set("ProductVersion", "");
    res.compile().expect("Failed to compile Windows resources");
}
