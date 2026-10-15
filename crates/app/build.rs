//! Windows 资源嵌入:图标 + 版本信息。
fn main() {
    #[cfg(windows)]
    {
        use winresource::WindowsResource;
        let mut res = WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "定时任务管理器");
        res.set("ProductName", "scheduled-task");
        res.set("OriginalFilename", "scheduled-task.exe");
        res.compile().expect("嵌入 Windows 资源失败");
    }
}
