fn main() {
    // 一次性安装最小 stderr logger（清理方案 v0.3 §5.4）：不装则 core 的 log::warn! 静默丢弃
    qingniao_cli::install_stderr_logger();
    std::process::exit(qingniao_cli::run());
}
