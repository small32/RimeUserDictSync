fn main() {
    #[cfg(windows)]
    {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon("weasel.ico");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=unable to embed icon: {error}");
        }
    }
}
