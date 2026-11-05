//! Put the mark on the executable, which is where Windows takes the taskbar and
//! Explorer icon from.

fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("../../assets/flex.ico");
        res.set("ProductName", "flex");
        res.set("FileDescription", "flex - a Windows shell for Rex servers");
        res.set("LegalCopyright", "MIT");
        if let Err(e) = res.compile() {
            eprintln!("flex: could not embed the icon: {e}");
        }
    }
}
