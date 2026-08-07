# PhiMakor 镜像协议 v1(桌面 editor ↔ wasm player)

## 连接
- 手机 → 电脑:`ws://<电脑IP>:8765/ws`(WebSocket,二进制帧)
- 电脑 → 手机第一帧:纹理清单(逐条),然后每帧快照
- 手机 → 电脑:控制文本帧(JSON)

## 纹理清单(握手,电脑 → 手机,每条一帧)
```
帧 0x00: [0x00][name_len:u16][name][len:u32][PNG 字节]
  槽位顺序固定:
    0 = 白色 1x1(程序生成,不发)
    1 = click.png      (kind 1 tap)
    2 = drag.png       (kind 4 drag)
    3 = flick.png      (kind 3 flick)
    4 = hold.png       (kind 2 hold, 含 body/head/tail)
    5 = hitfx.png      (fx 粒子)
    6 = line.png       (默认判定线)
    7+ = 谱面自定义线纹理(按线索引去重顺序)
帧 0xFF: [0xFF] 纹理清单结束
```

## 快照帧(电脑 → 手机,每帧一条)
```
[0x01]
  chart_time: f64
  dim:         f32          (背景压暗 0..1)
  line_count:  u16
  每线:
    pos:   f32 x2
    rot:   f32             (弧度)
    scale: f32 x2
    alpha: f32
    z:     i32
    tex:   u8              (槽位,默认 6)
    note_count: u16
    每 note:
      kind:    u8           (1 tap 2 hold 3 flick 4 drag)
      x:       f32          (relative[0])
      y:       f32          (relative[1], 已含 above 镜像符号)
      end_y:   f32          (hold 尾,非 hold 为 NaN → 写 f32::NAN)
      alpha:   f32
      scale:   f32
      tex:     u8           (kind 映射:1→1,2→4,3→3,4→2)
[0x02] fx 帧(可选,按需):t0: f64, count: u16, 每点 [x: f32, y: f32, rot: f32, age: f32]
```
> hold 的 head/tail/body 由 player 端按 end_y 拆三段(与桌面一致)。

## 控制(手机 → 电脑,文本 JSON 帧)
```
{"a":"pause"}           暂停/恢复(切换)
{"a":"seek","t":12.5}   seek 到音频秒
{"a":"ping"}            回应 pong
```

## 音频(HTTP)
- `GET /music/<文件名>` — 谱面音乐文件(支持 Range,手机 <audio> 播放)
- `GET /` — 播放器页面

## 约定
- 快照全量(无增量);30fps 上限,电脑端节流
- 网络序:大端(big-endian,手写打包)
