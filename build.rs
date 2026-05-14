fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set_manifest_file("assets/app.manifest");
        if let Err(e) = res.compile() {
            eprintln!("warning: winres compile failed: {}", e);
        }
    }
}
