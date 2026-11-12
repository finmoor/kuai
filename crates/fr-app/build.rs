//! 把图标和版本信息嵌进 exe。窗口左上角和任务栏的图标取的就是这里嵌进去的 1 号图标。

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/icon.ico")
        .set("ProductName", "Kuai")
        .set(
            "FileDescription",
            "Kuai：能打开服务器上仓库的 Git 客户端",
        );
    if let Err(err) = resource.compile() {
        // 没装 Windows SDK 时嵌不了图标，程序照样能用
        println!("cargo:warning=没有嵌入图标：{err}");
    }
}
