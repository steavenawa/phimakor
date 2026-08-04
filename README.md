# PhiMakor

<p style="font-size:36px;text-align:center;">PhiMakor!("Preview" ver) 全新来袭!</p>

PhiMakor 是一个 Rust + wgpu + Iced(tiny-skia) 的 Phigros Fanmade 谱面编辑器。

> 部分代码衍生自 [TeamFlos/phira](https://github.com/TeamFlos/phira)（GPL-3.0，详见 [ThankList.md](ThankList.md)）

## 运行

```sh
cargo run --release <谱面目录>
```

谱面目录需要包含 `chart.json`（RPE 格式）、音乐文件、曲绘以及 `info.json`（或 `info.txt`）。  
不传参数时会列出当前目录下可用的谱面。  
note资源包放在 `res/`，字体 `res/Exo2.ttf` 可选。

### 资源来源

`res/` 下的资源有版权考量，不入库。参考社区资源包，解压到 `res/` 下（应包含以下文件）：

```
click.ogg
click.png
click_mh.png
drag.ogg
drag.png
drag_mh.png
ending.mp3
Exo2.ttf
flick.ogg
flick.png
flick_mh.png
hit_fx.png
hold.png
hold_mh.png
info.yml
```

缺文件时程序会告警并降级（音符贴图缺失 → 画不出音符；hitsound 缺失 → 静默）。

## 快捷键

| 键 | 功能 |
|--------|-----------|
| Space | 播放 / 暂停 |
| ← → | 后退 / 前进 5 秒 |
| Tab | 切换 playfield 比例 |
| 滚轮 | 时间轴滚动 / 谱面时间拖拽 |
| Ctrl+滚轮 | 时间轴缩放 |
| Z / C | 上一条 / 下一条判定线（抬键触发） |
| F1 | 切换所有 UI 叠加层 |
| F3 | 右侧属性面板（Chart / Line / Settings） |
| F4 | 事件时间轴（Alpha / MoveX / MoveY / Rotate / Speed） |
| F5 | Note 预览面板 |
| Ctrl+F5 | 全线 Note 预览 |
| [ / ] | UI 缩放 0.5× – 2.0× |
| Esc | 退出 |

## 当前进度

note 预览、主谱面预览、事件预览、多面板信息查看。编辑器部分正在制作中。

## 关于本项目

本项目是指向性的 vibe coding 产物
但是其实感觉...额...反正我有这个需要()

## 关于性能

[//]:我chovy这里怎么在本地编辑的时候漏了一段?!
本项目的性能待测,但是现阶段可见性能并未深度优化.

[//]:绝对不是我懒奥,我真的不是不想测试,只是没找到实际的测试案例.

## License

本项目的初衷就是去做一个开源新兴前沿的谱面编辑器,亦可提供rust带来的轻量级以及wgpu技术带来的性能提升.因此(并且依照上游的参考代码许可证的传染性),此项目依照GPL-3.0开源.

[GPL-3.0](LICENSE)

---

[//]:#啊如果你看到我了,或者说对这个项目的维护感兴趣的话,欢迎给我的bilibili账户发送私信交流 (https://space.bilibili.com/385673259)
