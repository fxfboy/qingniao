fn main() {
    // 「关于」页「最近更新」用的打包时刻（Unix 秒，编译期常量）：
    // 同一个版本可能被多次打包，展示的必须是这份产物实际的打包时间。
    //
    // 这里刻意不用「可执行文件修改时间」：Windows 发布的 zip 只存「打包机器的本地时间」
    // 且不带时区，CI runner 是 UTC，用户（CST）解压后 mtime 会被当成自己的本地时间，
    // 结果整整差 8 小时、甚至显示成前一天。改成写入绝对时刻，运行时再按用户本地时区
    // 格式化即可避免这个偏移。
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=QINGNIAO_BUILD_EPOCH={epoch}");
    tauri_build::build()
}
