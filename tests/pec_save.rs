//! PEC 源谱面保存保护(bug 76ef60f9):open 时记录 source_is_pec,
//! save()/save_background() 拒绝覆写,`.pec` 文件保持 PEC 文本原样;
//! JSON 源谱面行为与现状一致(正常覆写)。

use std::fs;
use std::path::PathBuf;

use phimakor::core::edit::ChartDocument;

fn temp_chart_dir(tag: &str, info_chart: &str, chart_bytes: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "phimakor-pec-save-{}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("info.json"),
        format!(r#"{{"chart": "{info_chart}", "name": "fixture"}}"#),
    )
    .unwrap();
    fs::write(dir.join(info_chart), chart_bytes).unwrap();
    dir
}

#[test]
fn pec_source_save_rejected_and_file_unchanged() {
    // 最小合法 PEC:首行 offset,后续 bp/谱面行。
    let pec = "0\nbp 120 0\nn1 0 1 0 1 0\n";
    let dir = temp_chart_dir("pec", "chart.pec", pec);
    let mut doc = ChartDocument::open(&dir).unwrap();
    // 同步保存必须被拒绝,错误信息指明 PEC
    let err = doc.save().unwrap_err();
    assert!(
        err.to_string().contains("PEC"),
        "错误应指明 PEC 谱面,实际: {err}"
    );
    // 文件未被覆写(仍是 PEC 文本,不是 JSON)
    assert_eq!(fs::read(dir.join("chart.pec")).unwrap(), pec.as_bytes());
    // 后台保存路径同样不写 PEC 源文件(flush 后文件仍原样)
    doc.save_background();
    doc.flush().unwrap();
    assert_eq!(fs::read(dir.join("chart.pec")).unwrap(), pec.as_bytes());
}

#[test]
fn json_source_save_still_writes_json() {
    let json = r#"{"META":{"offset":0},"BPMList":[],"judgeLineList":[]}"#;
    let dir = temp_chart_dir("json", "chart.json", json);
    let mut doc = ChartDocument::open(&dir).unwrap();
    doc.save().unwrap();
    let s = String::from_utf8(fs::read(dir.join("chart.json")).unwrap()).unwrap();
    assert!(s.contains("\"META\""), "JSON 源谱面仍正常覆写为 JSON");
}
