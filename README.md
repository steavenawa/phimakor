# phimakor

把部分主要逻辑根据另一个完全剥离重写,渲染代码后端换成了wgpu，暂时还属于谱面预览播放器。
部分代码启发或衍生自 [TeamFlos/phira](https://github.com/TeamFlos/phira)（GPL-3.0，详见 [ThankList.md](ThankList.md)）。

## 运行

```sh
cargo run --release <谱面目录>
```

谱面目录含 `chart.json`谱面文件+ 音频 + 曲绘 + `info.json`（或谱面的 `info.txt`）。
皮肤资源包解压到启动目录的 `res/` 文件夹；字体 `res/Exo2.ttf` 可选。

## 按键

| 键 | 功能 |
|---|---|
| Space | 播放/暂停 |
| ← / → | 快退/快进 5 秒 |
| Tab | 切换 playfield 比例（3:2 → 16:9 → 4:3 → 1:1） |
| Esc | 退出 |

启动时附带一个调试小窗（time/fps/combo/线数等），关掉它不影响主程序。

## License

本项目的初衷就是去做一个开源新兴前沿的谱面编辑器,亦可提供rust带来的轻量级以及wgpu技术带来的性能提升.因此(并且依照上游的参考代码许可证的传染性),此项目依照GPL-3.0开源.

[GPL-3.0](LICENSE)

---

本项目其实是有指向性的vibe coding产物

如果你不喜欢它

看在我很可爱的份上可以不要拉踩我嘛(pwp)
