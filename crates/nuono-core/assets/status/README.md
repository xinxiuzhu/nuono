# 状态卡内嵌字体

`NuonoStatusSans.ttf` 从 [Google Fonts 的 Noto Sans SC](https://github.com/google/fonts/tree/main/ofl/notosanssc) 生成。随附 `OFL.txt` 为原许可证；字体保留原版权声明，并将修改版名称改为 Nuono Status Sans。

来源文件 `NotoSansSC[wght].ttf` 的 SHA-256：
`a3041811a78c361b1de50f953c805e0244951c21c5bd412f7232ef0d899af0da`。

固定字重 450，保留 GB2312 中文、ASCII、Latin-1、常用标点、假名和全角字符。不依赖部署机器的字体，其他字形显示替代符。

维护时在临时 Python 环境安装 `fonttools`，执行：

```sh
python xtask/scripts/subset_status_font.py NotoSansSC.ttf crates/nuono-core/assets/status/NuonoStatusSans.ttf
```
