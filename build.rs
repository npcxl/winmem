//! 构建脚本：把 `winmem.manifest` 通过链接器参数嵌入可执行文件。
//!
//! 嵌入的 manifest 负责：
//! 1. 激活 Common Controls 6.0（`Microsoft.Windows.Common-Controls` v6），
//!    使按钮、列表、滚动条等控件使用**系统当前视觉样式** —— Win11 上是
//!    Fluent 圆角样式，Win10 上就是 Win10 的样式，无需任何第三方依赖。
//! 2. 声明兼容 Windows 7 ~ 11，并开启高 DPI 感知与长路径支持。
//!
//! 该方案只使用 MSVC 链接器自带能力（`/MANIFEST:EMBED` + `/MANIFESTINPUT:`），
//! 不引入任何 crate 或资源编译器。

fn main() {
    println!("cargo:rerun-if-changed=winmem.manifest");

    // 相对路径：链接器的工作目录即包根目录，避免盘符冒号干扰选项解析。
    for bin in ["winmem", "winmem-gui"] {
        println!("cargo:rustc-link-arg-bin={bin}=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin={bin}=/MANIFESTINPUT:winmem.manifest");
    }
}
