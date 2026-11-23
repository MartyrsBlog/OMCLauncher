fn main() {
    // 编译 .slint 界面（编译期校验 + 代码生成；live-preview 由此文件入口打开）
    slint_build::compile("ui/app-window.slint").expect("Slint 界面编译失败");
}
