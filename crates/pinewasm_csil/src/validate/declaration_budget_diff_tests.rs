//! Test-only copies of the validator bodies, used as expected-value checks.
use super::*;
use pinewasm_hir::{InputActiveDirect, TaFunc, TaStateDecl};

fn expected_validate(program: &CsilProgram) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if program_prefix::check(
        program,
        &mut program_prefix::VecSink {
            program,
            errors: &mut errors,
        },
    ) == program_prefix::Flow::Stop
    {
        return Err(errors);
    }

    let before_ta_states = errors.len();
    expected_validate_ta_states(program, &mut errors);
    validate_ta_input_bindings(program, &mut errors);
    if errors.len() > before_ta_states {
        // The per-function checks look ta_states entries up; do not run
        // them over a malformed table.
        return Err(errors);
    }

    let before_inputs = errors.len();
    expected_validate_inputs(program, &mut errors);
    if errors.len() > before_inputs {
        // The per-function checks look inputs entries up for the READ_INPUT
        // push type; do not run them over a malformed table.
        return Err(errors);
    }

    // Each ta state id may be used by at most one CALL_TA site across the
    // whole program (per-call-site state must not be shared).
    struct TaUses<'a> {
        program: &'a CsilProgram,
        users: Vec<Option<(String, u32)>>,
        errors: &'a mut Vec<ValidationError>,
    }
    impl global_scan::TaUser for TaUses<'_> {
        fn observe(mut self, function: usize, index: usize, state: u32) -> Self {
            let func = &self.program.functions[function];
            let offset = instruction_offset(index);
            if let Some(user) = self.users.get_mut(state as usize) {
                match user {
                    None => *user = Some((func.name.clone(), offset)),
                    Some((first_func, first_offset)) => {
                        self.errors.push(ValidationError::AtInstruction {
                            function: func.name.clone(),
                            offset,
                            message: format!(
                                "ta state {state} is already used by `{first_func}` at \
                                     {first_offset:04}; each state id belongs to exactly one \
                                     call site"
                            ),
                        });
                    }
                }
            }
            // Out-of-range states are reported by the structural pass.
            self
        }
    }
    let _ = global_scan::ta_users(
        program,
        TaUses {
            program,
            users: vec![None; program.ta_states.len()],
            errors: &mut errors,
        },
    );

    for func in &program.functions {
        validate_function(program, func, &mut errors);
    }

    // A reference is untrusted wire data. Admit it only when it is bounded,
    // independently executable, and reproduces the optimized function under
    // the compiler's deterministic pass sequence.
    for (index, func) in program.functions.iter().enumerate() {
        let Some(reference) = &func.budget_reference else {
            continue;
        };
        if index != 0
            || !crate::budget_reference_version_known(reference.version)
            || !crate::budget_reference_within_limit(reference)
            || reference.code.len() != reference.source_map.len()
        {
            errors.push(ValidationError::Structure {
                function: func.name.clone(),
                message: "invalid or oversized budget reference".to_string(),
            });
            continue;
        }
        let mut reference_program = program.clone();
        for function in &mut reference_program.functions {
            function.budget_reference = None;
        }
        let reference_func = &mut reference_program.functions[index];
        reference_func.code.clone_from(&reference.code);
        reference_func.source_map.clone_from(&reference.source_map);
        reference_func.max_stack = reference.max_stack;
        if expected_validate(&reference_program).is_err()
            || !crate::lower::matches_budget_reference(reference, func, &program.series_slots)
        {
            errors.push(ValidationError::Structure {
                function: func.name.clone(),
                message: "budget reference does not validate or reproduce optimized code"
                    .to_string(),
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn expected_validate_ta_states(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    for (index, decl) in program.ta_states.iter().enumerate() {
        let mut bad = |message: String| errors.push(ValidationError::TaState { index, message });
        if !decl.func.is_engine() {
            bad(format!(
                "`{}` is slot-based, not an engine function",
                decl.func.name()
            ));
            continue;
        }
        let rules = decl.func.engine_param_rules();
        if decl.params.len() != rules.len() {
            bad(format!(
                "`{}` takes {} const parameter(s), found {}",
                decl.func.name(),
                rules.len(),
                decl.params.len()
            ));
            continue;
        }
        for (i, (rule, value)) in rules.iter().zip(&decl.params).enumerate() {
            match rule {
                TaParamRule::Length { min } => {
                    if !(*min..=MAX_WINDOW_LENGTH as i64).contains(value) {
                        bad(format!(
                            "`{}` param {i}: window length {value} out of range \
                             ({min}..={MAX_WINDOW_LENGTH})",
                            decl.func.name()
                        ));
                    }
                }
                TaParamRule::IntRange { min, max } => {
                    if !(*min..=*max).contains(value) {
                        bad(format!(
                            "`{}` param {i}: {value} out of range ({min}..={max})",
                            decl.func.name()
                        ));
                    }
                }
                TaParamRule::FloatBits { positive } => {
                    let f = f64::from_bits(u64::from_ne_bytes(value.to_ne_bytes()));
                    if !f.is_finite() {
                        bad(format!(
                            "`{}` param {i}: bit pattern does not decode to a finite float",
                            decl.func.name()
                        ));
                    } else if *positive && f <= 0.0 {
                        bad(format!(
                            "`{}` param {i}: must be greater than zero, found {f}",
                            decl.func.name()
                        ));
                    }
                }
                TaParamRule::Percent => {
                    let f = f64::from_bits(u64::from_ne_bytes(value.to_ne_bytes()));
                    if !f.is_finite() || !(0.0..=100.0).contains(&f) {
                        bad(format!(
                            "`{}` param {i}: percentage bit pattern must decode to a \
                             finite float in 0..=100",
                            decl.func.name()
                        ));
                    }
                }
                TaParamRule::Bool01 => {
                    if !(0..=1).contains(value) {
                        bad(format!(
                            "`{}` param {i}: flag must be 0 or 1, found {value}",
                            decl.func.name()
                        ));
                    }
                }
            }
        }
    }
}

fn expected_validate_inputs(program: &CsilProgram, errors: &mut Vec<ValidationError>) {
    for (index, decl) in program.inputs.iter().enumerate() {
        let mut bad = |message: String| errors.push(ValidationError::Input { index, message });
        if decl.id as usize != index {
            bad(format!("id {} does not match its index {index}", decl.id));
        }
        if !value_matches_kind(&decl.defval, decl.kind) {
            bad(format!(
                "defval does not match declared kind `input.{}`",
                decl.kind.describe()
            ));
        }
        if let Some(message) = invalid_input_value_domain(&decl.defval) {
            bad(format!("defval {message}"));
        }
        for (label, value) in [
            ("minval", decl.constraints.min.as_ref()),
            ("maxval", decl.constraints.max.as_ref()),
            ("step", decl.constraints.step.as_ref()),
        ] {
            if let Some(v) = value {
                if !value_matches_kind(v, decl.kind) {
                    bad(format!("{label} does not match declared kind"));
                }
                if let Some(message) = invalid_input_value_domain(v) {
                    bad(format!("{label} {message}"));
                }
            }
        }
        for opt in &decl.constraints.options {
            if !value_matches_kind(opt, decl.kind) {
                bad("an options entry does not match declared kind".to_string());
            }
            if let Some(message) = invalid_input_value_domain(opt) {
                bad(format!("an options entry {message}"));
            }
        }
        // min/max/step are numeric-only. Options additionally belong to the
        // string and enum dropdown forms.
        let has_numeric_bounds = decl.constraints.min.is_some()
            || decl.constraints.max.is_some()
            || decl.constraints.step.is_some();
        if !matches!(decl.kind, InputKind::Int | InputKind::Float) && has_numeric_bounds {
            bad(format!(
                "`input.{}` does not take numeric constraints",
                decl.kind.describe()
            ));
        }
        if !matches!(
            decl.kind,
            InputKind::Int | InputKind::Float | InputKind::String | InputKind::Enum
        ) && !decl.constraints.options.is_empty()
        {
            bad(format!(
                "`input.{}` does not take an options list",
                decl.kind.describe()
            ));
        }
        if has_numeric_bounds && !decl.constraints.options.is_empty() {
            bad("numeric bounds/step and options are mutually exclusive".to_string());
        }

        if let (Some(default), Some(minimum)) = (
            numeric_input_value(&decl.defval),
            decl.constraints.min.as_ref().and_then(numeric_input_value),
        ) {
            if default < minimum {
                bad("defval is below minval".to_string());
            }
        }
        if let (Some(default), Some(maximum)) = (
            numeric_input_value(&decl.defval),
            decl.constraints.max.as_ref().and_then(numeric_input_value),
        ) {
            if default > maximum {
                bad("defval is above maxval".to_string());
            }
        }
        if !decl.constraints.options.is_empty()
            && !decl
                .constraints
                .options
                .iter()
                .any(|option| input_values_share_domain_value(&decl.defval, option, decl.kind))
        {
            bad("options do not contain the declared default".to_string());
        }
        if let InputValue::Enum {
            name: enum_name,
            index: default_index,
            ..
        } = &decl.defval
        {
            let mut seen_indexes = Vec::new();
            let mut seen_members: Vec<&str> = Vec::new();
            let mut contains_default = decl.constraints.options.is_empty();
            for option in &decl.constraints.options {
                let InputValue::Enum {
                    name,
                    index,
                    member,
                    ..
                } = option
                else {
                    continue;
                };
                if name != enum_name {
                    bad(format!(
                        "enum option `{name}` does not match declaring enum `{enum_name}`"
                    ));
                }
                if seen_indexes.contains(index) {
                    bad(format!("duplicate enum option index {index}"));
                }
                seen_indexes.push(*index);
                if !member.is_empty() {
                    if seen_members.contains(&member.as_str()) {
                        bad(format!("duplicate enum option member `{member}`"));
                    }
                    seen_members.push(member);
                }
                contains_default |= name == enum_name && index == default_index;
            }
            if !contains_default {
                bad("enum options do not contain the declared default member".to_string());
            }
        }
        let mut previous_dependency = None;
        for dependency in &decl.active.dependencies {
            if *dependency as usize >= index {
                bad(format!(
                    "active dependency {dependency} must reference a prior input"
                ));
            }
            if previous_dependency.is_some_and(|previous| previous >= *dependency) {
                bad("active dependencies must be sorted and unique".to_string());
            }
            previous_dependency = Some(*dependency);
        }
        if let Some(direct) = decl.active.direct {
            if direct.input as usize >= index {
                bad(format!(
                    "direct active dependency {} must reference a prior input",
                    direct.input
                ));
            } else if program.inputs[direct.input as usize].kind != InputKind::Bool {
                bad(format!(
                    "direct active dependency {} must reference a bool input",
                    direct.input
                ));
            }
            if !decl.active.dependencies.contains(&direct.input) {
                bad(format!(
                    "direct active dependency {} is missing from dependencies",
                    direct.input
                ));
            }
        }
        if let Some(expression) = &decl.active.expression {
            let mut expression_dependencies = Vec::new();
            let mut expression_nodes = 0;
            validate_active_expression(
                expression,
                index,
                1,
                &mut expression_nodes,
                &mut expression_dependencies,
                &mut bad,
            );
            expression_dependencies.sort_unstable();
            expression_dependencies.dedup();
            if expression_dependencies != decl.active.dependencies {
                bad(format!(
                    "active expression dependencies {:?} do not match declared dependencies {:?}",
                    expression_dependencies, decl.active.dependencies
                ));
            }
        }
    }
}

fn compile(source: &str) -> CsilProgram {
    let parsed = pinewasm_parse::parse(source);
    assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
    let analysis = pinewasm_semantics::analyze(&parsed.program);
    assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
    crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
}
fn plain() -> CsilProgram {
    compile("//@version=6\nindicator(\"decl seam\")\nplot(close)")
}
fn input_program() -> CsilProgram {
    compile("//@version=6\nindicator(\"decl seam\")\nenabled = input.bool(true, \"enabled\")\nlength = input.int(14, \"length\")\nplot(close)")
}
fn same(program: &CsilProgram) {
    assert_eq!(validate(program), expected_validate(program));
    let mut ta_old = vec![ValidationError::Metadata("sentinel".into())];
    let mut ta_new = vec![ValidationError::Metadata("sentinel".into())];
    expected_validate_ta_states(program, &mut ta_old);
    validate_ta_states(program, &mut ta_new);
    assert_eq!(ta_new, ta_old);
    let mut input_old = vec![ValidationError::Metadata("sentinel".into())];
    let mut input_new = vec![ValidationError::Metadata("sentinel".into())];
    expected_validate_inputs(program, &mut input_old);
    validate_inputs(program, &mut input_new);
    assert_eq!(input_new, input_old);
}
#[derive(Debug, PartialEq)]
struct Trace {
    sentinel: u64,
    events: Vec<usize>,
}
impl declaration_scan::TaDeclaration for Trace {
    fn observe(mut self, index: usize) -> Self {
        self.events.push(index);
        self
    }
}
impl declaration_scan::InputDeclaration for Trace {
    fn observe(mut self, index: usize) -> Self {
        self.events.push(index);
        self
    }
}
impl budget_scan::Reference for Trace {
    fn observe(mut self, index: usize) -> Self {
        self.events.push(index);
        self
    }
}
fn trace() -> Trace {
    Trace {
        sentinel: 0x1234_5678,
        events: Vec::new(),
    }
}
#[test]
fn declaration_budget_scan_empty_and_presence_order() {
    let mut program = plain();
    assert_eq!(declaration_scan::ta_states(&program, trace()), trace());
    assert_eq!(declaration_scan::inputs(&program, trace()), trace());
    assert_eq!(budget_scan::references(&program, trace()), trace());
    program = input_program();
    assert_eq!(
        declaration_scan::inputs(&program, trace()).events,
        vec![0, 1]
    );
    program.ta_states = vec![
        TaStateDecl {
            func: TaFunc::Sma,
            params: vec![14]
        };
        3
    ];
    assert_eq!(
        declaration_scan::ta_states(&program, trace()).events,
        vec![0, 1, 2]
    );
    let function = program.functions[0].clone();
    program.functions = vec![function; 4];
    for index in [1, 3] {
        program.functions[index].budget_reference = Some(crate::BudgetReference {
            version: 1,
            code: Vec::new(),
            source_map: Vec::new(),
            max_stack: 0,
        });
    }
    assert_eq!(
        budget_scan::references(&program, trace()).events,
        vec![1, 3]
    );
}
#[test]
fn declaration_ta_rules_and_failures_keep_their_order() {
    let base = plain();
    for func in TaFunc::ALL {
        let rules = func.engine_param_rules();
        let params: Vec<i64> = rules
            .iter()
            .map(|rule| match rule {
                TaParamRule::Length { min } | TaParamRule::IntRange { min, .. } => *min,
                TaParamRule::FloatBits { .. } | TaParamRule::Percent => {
                    i64::from_ne_bytes(1.0f64.to_bits().to_ne_bytes())
                }
                TaParamRule::Bool01 => 1,
            })
            .collect();
        for mode in 0..5 {
            let mut program = base.clone();
            let mut values = params.clone();
            match mode {
                1 => values.push(999),
                2 => values.fill(i64::MIN),
                3 => values.fill(i64::MAX),
                4 => values.fill(i64::from_ne_bytes(f64::INFINITY.to_bits().to_ne_bytes())),
                _ => {}
            }
            program.ta_states = vec![TaStateDecl {
                func,
                params: values,
            }];
            // Per-function malformed code must remain suppressed by malformed TA declarations.
            program.functions[0].code = vec![CsilInstr::Pop, CsilInstr::Return];
            program.functions[0].source_map.truncate(2);
            same(&program);
            if let Err(errors) = validate(&program) {
                if errors
                    .iter()
                    .any(|e| matches!(e, ValidationError::TaState { .. }))
                {
                    assert!(errors
                        .iter()
                        .all(|e| !matches!(e, ValidationError::AtInstruction { .. })));
                }
            }
        }
    }
}
#[test]
fn declaration_inputs_domains_dependencies_and_enum_options_match_baseline() {
    let base = input_program();
    for mode in 0..12 {
        let mut program = base.clone();
        let decl = &mut program.inputs[1];
        match mode {
            1 => decl.id = 999,
            2 => decl.defval = InputValue::Float(f64::NAN),
            3 => decl.constraints.min = Some(InputValue::Int(30)),
            4 => decl.constraints.max = Some(InputValue::Int(-30)),
            5 => {
                decl.constraints.min = Some(InputValue::Bool(false));
                decl.constraints.options = vec![InputValue::Int(1)];
            }
            6 => decl.active.dependencies = vec![1, 0, 0],
            7 => {
                decl.active.direct = Some(InputActiveDirect {
                    input: 1,
                    inverted: false,
                });
            }
            8 => {
                decl.active.dependencies = vec![0];
                decl.active.expression = Some(InputActiveExpr::Input { input: 1 });
            }
            9 => {
                decl.kind = InputKind::Bool;
                decl.constraints.step = Some(InputValue::Int(1));
            }
            10 => {
                decl.kind = InputKind::Enum;
                decl.defval = InputValue::Enum {
                    name: "E".into(),
                    title: Some("A".into()),
                    index: 1,
                    member: "A".into(),
                };
                decl.constraints.options = vec![decl.defval.clone(), decl.defval.clone()];
            }
            11 => {
                decl.defval = InputValue::Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    t: 101.0,
                };
            }
            _ => {}
        }
        same(&program);
    }
}
fn reference(program: &CsilProgram) -> crate::BudgetReference {
    let f = &program.functions[0];
    crate::BudgetReference {
        version: 1,
        code: f.code.clone(),
        source_map: f.source_map.clone(),
        max_stack: f.max_stack,
    }
}
#[test]
fn budget_some_none_recursive_failure_and_structural_precedence_match_baseline() {
    let base = plain();
    same(&base);
    for mode in 0..7 {
        let mut program = base.clone();
        let mut budget = reference(&program);
        match mode {
            1 => budget.version = 99,
            2 => budget.source_map.clear(),
            3 => budget.code = vec![CsilInstr::Pop; budget.source_map.len()],
            4 => budget.max_stack = 0,
            5 => {
                program.functions.push(program.functions[0].clone());
                program.functions[1].budget_reference = Some(budget.clone());
            }
            6 => {
                budget.version = 99;
                program.functions[0].code[0] = CsilInstr::Pop;
            }
            _ => {}
        }
        program.functions[0].budget_reference = Some(budget);
        same(&program);
    }
    let mut malformed = base.clone();
    let mut budget = reference(&malformed);
    budget.version = 99;
    budget.code = vec![CsilInstr::Pop; budget.source_map.len()];
    malformed.functions[0].budget_reference = Some(budget);
    let errors = validate(&malformed).expect_err("rejects malformed version before recursion");
    assert!(errors.iter().any(|e| matches!(e, ValidationError::Structure { message, .. } if message == "invalid or oversized budget reference")));
    assert!(errors.iter().all(|e| !matches!(e, ValidationError::Structure { message, .. } if message == "budget reference does not validate or reproduce optimized code")));
}
