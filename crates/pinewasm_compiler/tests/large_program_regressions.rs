use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_vm::{run, MarketData, RunOptions};

#[test]
fn large_palettes_point_wrappers_and_intrabar_tuples_compile() {
    let palette = std::iter::repeat_n("color.red", 64)
        .collect::<Vec<_>>()
        .join(",");
    for source in [
        format!("indicator(\"palette\")\np=array.from({palette})\nplot(p.size())\n"),
        "indicator(\"point wrapper\")\npt(int x,float y)=>chart.point.from_index(x,y)\nline.new(pt(0,1),pt(1,2))\nlabel.new(pt(1,2),\"label\")\n".into(),
        "indicator(\"intrabar tuples\")\ntf=input.timeframe(\"5\")\n[h,l,v]=request.security_lower_tf(syminfo.tickerid,tf,[high,low,volume])\nplot(h.size()+l.size()+v.size())\n".into(),
    ] {
        let result = compile(&source, CompileOptions::default());
        assert!(result.program.is_some(), "{source}: {:?}", result.diagnostics);
        let data = MarketData { open: vec![1.0], high: vec![2.0], low: vec![0.0], close: vec![1.0], volume: vec![10.0], ..MarketData::default() };
        let outcome = run(&result.program.unwrap(), &data, &RunOptions::default());
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    }
}
