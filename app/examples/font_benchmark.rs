//! Opt-in timing/regression tool, not part of the shipped app.
//! Clears font overrides in this process only to measure the OS fallback path.
//! UI timings include snapshot readback and are not end-to-end VSync FPS.
use serde_json::{Value, json};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    cell::{Cell, RefCell},
    io::Write,
    time::Instant,
};
slint::slint! { export {App,Row,PaneInfo,Columns} from "src/ui.slint"; }

const COUNT: usize = 5000;
const PAGE: usize = 24;
fn memory() -> Value {
    use windows::Win32::System::{ProcessStatus::*, Threading::*};
    let mut m = PROCESS_MEMORY_COUNTERS_EX::default();
    m.cb = std::mem::size_of_val(&m) as u32;
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut m as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            m.cb,
        )
        .unwrap();
    }
    json!({"private_bytes":m.PrivateUsage,"working_set_bytes":m.WorkingSetSize,"peak_working_set_bytes":m.PeakWorkingSetSize})
}
fn process_cpu_ms() -> f64 {
    use windows::Win32::{Foundation::FILETIME, System::Threading::*};
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).unwrap();
    }
    let time = |v: FILETIME| ((v.dwHighDateTime as u64) << 32) | v.dwLowDateTime as u64;
    (time(k) + time(u)) as f64 / 10000.
}
fn names(kind: &str) -> Vec<String> {
    let samples = if kind == "plain" {
        vec!["日本語カタカナ_漢字_ABC_xyz_0123456789".to_string()]
    } else if kind == "diverse" {
        (0..2048)
            .map(|i| format!("拡張_{}", char::from_u32(0x20000 + i).unwrap()))
            .collect()
    } else {
        [
            "日本語_ABC_012345",
            "简体_难汉",
            "繁體中文_龍門",
            "한국어",
            "拡張_𠀀",
            "記号_⟰⨌",
            "絵文字_😀",
            "通常_文書ファイル",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    };
    (0..COUNT)
        .map(|i| format!("{:05}_{}.txt", i, samples[i % samples.len()]))
        .collect()
}
fn cpu(kind: &str) -> Value {
    use parley::style::{FontFamily, FontFamilyName};
    use parley::{FontContext, LayoutContext, StyleProperty};
    let names = names(kind);
    let mut fc = FontContext::default();
    let mut lc = LayoutContext::<[u8; 4]>::new();
    let initial = memory();
    let mut stages = vec![];
    for pass in 0..3 {
        let cpu0 = process_cpu_ms();
        let t = Instant::now();
        let mut missing = 0usize;
        let mut width_sum = 0f64;
        let mut glyph_sum = 0u64;
        for text in &names {
            let mut b = lc.ranged_builder(&mut fc, text, 1.0, true);
            b.push_default(StyleProperty::FontFamily(FontFamily::Single(
                FontFamilyName::named("Meiryo UI"),
            )));
            b.push_default(StyleProperty::FontSize(13.0));
            let mut layout = b.build(text);
            layout.break_all_lines(None);
            for line in layout.lines() {
                for item in line.items() {
                    if let parley::layout::PositionedLayoutItem::GlyphRun(run) = item {
                        for glyph in run.glyphs() {
                            missing += (glyph.id == 0) as usize;
                            glyph_sum = glyph_sum.wrapping_add(glyph.id as u64);
                        }
                    }
                }
            }
            width_sum += layout.width() as f64;
            std::hint::black_box(layout.width());
        }
        stages.push(json!({"pass":pass,"process_cpu_ms":process_cpu_ms()-cpu0,"ms":t.elapsed().as_secs_f64()*1000.,"missing_glyphs":missing,"width_sum":width_sum,"glyph_sum":glyph_sum,"memory":memory()}));
    }
    json!({"mode":"cpu","corpus":kind,"names":COUNT,"initial_memory":initial,"stages":stages})
}
fn sample_rows(names: &[String], start: usize) -> Vec<Row> {
    names[start..(start + PAGE).min(names.len())]
        .iter()
        .map(|n| Row {
            name: n.trim_end_matches(".txt").into(),
            ext: "txt".into(),
            size: "1.00 KB".into(),
            date: "2026-11-08".into(),
            time: "12:00".into(),
            kind: "Text Document".into(),
            ..Row::default()
        })
        .collect()
}
fn frame(ui: &App, names: &[String], start: usize) -> f64 {
    let t = Instant::now();
    let model = ModelRc::new(VecModel::from(sample_rows(names, start)));
    ui.set_p0_rows(model.clone());
    ui.set_p1_rows(model);
    let shot = ui.window().take_snapshot().unwrap();
    std::hint::black_box(shot.as_slice()[0]);
    t.elapsed().as_secs_f64() * 1000.
}
fn distribution(times: &[f64]) -> Value {
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({"frames":times.len(),"total_ms":times.iter().sum::<f64>(),"median_ms":sorted[sorted.len()/2],"p95_ms":sorted[((sorted.len()-1) as f64*0.95) as usize],"min_ms":sorted[0],"max_ms":sorted[sorted.len()-1]})
}
fn ui_bench(kind: &str, path: String) -> Result<(), Box<dyn std::error::Error>> {
    let names = names(kind);
    let ui = App::new()?;
    ui.window().set_size(slint::PhysicalSize::new(1200, 720));
    ui.set_font_family("Meiryo UI".into());
    ui.set_font_size(13.);
    ui.set_p0_path("D:\\FontBench".into());
    ui.set_p1_path("D:\\FontBench".into());
    ui.set_columns(Columns {
        size: false,
        date: false,
        time: false,
        attr: false,
        kind: false,
    });
    let info = PaneInfo {
        folders: "0".into(),
        files: COUNT.to_string().into(),
        widest_name: "00000_日本語_ABC_012345".into(),
        widest_kind: "Text Document".into(),
        ..PaneInfo::default()
    };
    ui.set_p0_info(info.clone());
    ui.set_p1_info(info);
    let weak = ui.as_weak();
    let step = Cell::new(0usize);
    let stages = RefCell::new(vec![]);
    let mode = kind.to_string();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated,std::time::Duration::from_millis(250),move||{
        let ui=weak.unwrap();let mem0=memory();let cpu0=process_cpu_ms();let t=Instant::now();let mut times=vec![];
        let label=match step.get(){
            0=>{times.push(frame(&ui,&names,0));"first_page"},
            1|2=>{for offset in (0..names.len()).step_by(PAGE){times.push(frame(&ui,&names,offset));}if step.get()==1{"first_sweep_5000"}else{"warm_sweep_5000"}},
            3=>{for offset in 0..120{times.push(frame(&ui,&names,offset));}"scroll_120_rows"},
            4=>{for _ in 0..120{times.push(frame(&ui,&names,120));}"same_page_120_frames"},
            _=>{
                let report=json!({"mode":"real_app_ui_snapshot","corpus":mode,"names":COUNT,"visible_rows_per_pane":PAGE,"size":[1200,720],"stages":*stages.borrow()});
                std::fs::write(&path,serde_json::to_vec_pretty(&report).unwrap()).unwrap();
                let shot=ui.window().take_snapshot().unwrap();let mut f=std::fs::File::create(format!("{path}.ppm")).unwrap();write!(f,"P6\n{} {}\n255\n",shot.width(),shot.height()).unwrap();f.write_all(&shot.as_slice().iter().flat_map(|p|[p.r,p.g,p.b]).collect::<Vec<_>>()).unwrap();slint::quit_event_loop().unwrap();return;
            }
        };
        stages.borrow_mut().push(json!({"label":label,"process_cpu_ms":process_cpu_ms()-cpu0,"wall_ms":t.elapsed().as_secs_f64()*1000.,"draw":distribution(&times),"memory_before":mem0,"memory_after":memory()}));step.set(step.get()+1);
    });
    ui.run()?;
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        std::env::remove_var("SLINT_FONT_PATH");
        std::env::remove_var("SLINT_DEFAULT_FONT");
        std::env::set_var("SLINT_SCALE_FACTOR", "1");
    }
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("cpu");
    let kind = args.get(2).map(String::as_str).unwrap_or("mixed");
    let path = args.get(3).cloned().unwrap_or("bench.json".into());
    if let Some(parent) = std::path::Path::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    if mode == "cpu" {
        std::fs::write(path, serde_json::to_vec_pretty(&cpu(kind))?)?;
    } else {
        ui_bench(kind, path)?;
    }
    Ok(())
}
