use std::path::PathBuf;
use crate::ipc_client::Endpoint;

pub struct Options {
    pub demo: bool,
    pub page: i32,
    pub width: u32,
    pub height: u32,
    pub dark: Option<bool>,
    pub dump_png: Option<PathBuf>,
    pub endpoint: Endpoint,
    pub check_layout: bool,
    pub help: bool,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self { demo: false, page: 0, width: 1080, height: 700, dark: None, dump_png: None, endpoint: Endpoint::new(None).map_err(|e| e.to_string())?, check_layout: false, help: false };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--demo" => options.demo = true,
                "--check-layout" => options.check_layout = true,
                "--help" | "-h" => options.help = true,
                "--page" | "--width" | "--height" | "--theme" | "--dump-png" | "--pipe" => {
                    let value = args.next().ok_or_else(|| format!("{arg} 缺少参数"))?;
                    match arg.as_str() {
                        "--page" => options.page = value.parse().ok().filter(|n| (0..8).contains(n)).ok_or("--page 取值为 0..7")?,
                        "--width" => options.width = dimension(&value)?,
                        "--height" => options.height = dimension(&value)?,
                        "--theme" => options.dark = match value.as_str() { "dark" => Some(true), "light" => Some(false), "system" => None, _ => return Err("--theme 取值为 system/light/dark".into()) },
                        "--dump-png" => { if value.is_empty() { return Err("PNG 路径不能为空".into()); } options.dump_png = Some(value.into()); }
                        "--pipe" => options.endpoint = Endpoint::new(Some(value)).map_err(|e| e.to_string())?,
                        _ => unreachable!(),
                    }
                }
                _ => return Err(format!("未知参数：{arg}")),
            }
        }
        if u64::from(options.width) * u64::from(options.height) > 32_000_000 { return Err("导出尺寸不能超过 3200 万像素".into()); }
        Ok(options)
    }
}

fn dimension(value: &str) -> Result<u32, String> {
    value.parse().ok().filter(|n| (320..=8192).contains(n)).ok_or_else(|| "尺寸取值为 320..8192 逻辑像素".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Options, String> { Options::parse(args.iter().map(|s| (*s).into())) }
    #[test]
    fn export_options_are_validated() {
        let o = parse(&["--demo", "--page", "7", "--width", "1280", "--height", "800", "--theme", "light", "--dump-png", "page.png"]).unwrap();
        assert_eq!((o.width, o.height, o.page, o.dark), (1280, 800, 7, Some(false)));
        assert!(o.demo && o.dump_png.is_some());
        for args in [&["--page", "8"][..], &["--width", "0"], &["--height"], &["--theme", "neon"], &["--bogus"]] { assert!(parse(args).is_err()); }
    }
}
