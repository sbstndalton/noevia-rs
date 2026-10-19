//! Incremental policy over opaque host references. No job/evidence JSON crosses this API.
//! A fixed register frame resumes after each mechanical host read/call. Array callbacks use
//! independent small programs, so work is linear and getters may re-enter another report.
use super::*;

pub(super) const FRAME_LIMIT: usize = 64 * 1024;
const REGISTERS: usize = 32;
const TESTS: &str = match CHECK_NAMES {
    [name, _, _, _, _] => name,
};
const ARTIFACTS: &str = match CHECK_NAMES {
    [_, name, _, _, _] => name,
};
const PLAN: &str = match CHECK_NAMES {
    [_, _, name, _, _] => name,
};
const UNRESOLVED: &str = match CHECK_NAMES {
    [_, _, _, name, _] => name,
};
const CHECKPOINT: &str = match CHECK_NAMES {
    [_, _, _, _, name] => name,
};
#[derive(Clone)]
enum Test {
    Truth(usize),
    Kind(usize, &'static str),
    Text(usize, &'static str),
    Nullish(usize),
    Not(Box<Test>),
}
#[derive(Clone)]
enum Key {
    Name(&'static str),
    Register(usize),
}
#[derive(Clone)]
enum Part {
    Text(&'static str),
    Raw(usize),
}
#[derive(Clone)]
enum Instruction {
    Literal(usize, Value),
    Get(usize, usize, Key),
    Method(
        usize,
        usize,
        &'static str,
        Vec<usize>,
        Option<(usize, Vec<usize>)>,
    ),
    Evaluate(usize, usize, Vec<usize>),
    Object(usize, Vec<(&'static str, usize)>),
    Array(usize, Vec<usize>),
    Template(usize, Vec<Part>),
    Mechanical(usize, &'static str, Vec<usize>),
    Branch(Test, usize, usize),
    Jump(usize),
    Return(usize),
    Throw(&'static str, u16),
    ThrowType(&'static str),
}
#[derive(Default)]
struct Program(Vec<Instruction>);
impl Program {
    fn push(&mut self, instruction: Instruction) {
        self.0.push(instruction);
    }
    fn literal(&mut self, reg: usize, text: &str) {
        self.push(Instruction::Literal(reg, string(text)));
    }
    fn boolean(&mut self, reg: usize, value: bool) {
        self.push(Instruction::Literal(reg, Value::Bool(value)));
    }
    fn get(&mut self, dst: usize, src: usize, key: &'static str) {
        self.push(Instruction::Get(dst, src, Key::Name(key)));
    }
    fn method(&mut self, dst: usize, src: usize, name: &'static str, callback: usize) {
        self.push(Instruction::Method(
            dst,
            src,
            name,
            vec![],
            Some((callback, vec![])),
        ));
    }
    fn branch(&mut self, test: Test, yes: impl FnOnce(&mut Self), no: impl FnOnce(&mut Self)) {
        let branch = self.0.len();
        self.push(Instruction::Jump(0));
        let yes_start = self.0.len();
        yes(self);
        let jump = self.0.len();
        self.push(Instruction::Jump(0));
        let no_start = self.0.len();
        no(self);
        let end = self.0.len();
        if let Some(slot) = self.0.get_mut(branch) {
            *slot = Instruction::Branch(test, yes_start, no_start);
        }
        if let Some(slot) = self.0.get_mut(jump) {
            *slot = Instruction::Jump(end);
        }
    }
    fn fallback(&mut self, dst: usize, src: usize, empty_array: bool) {
        self.branch(
            Test::Truth(src),
            |p| p.push(Instruction::Mechanical(dst, "identity", vec![src])),
            |p| {
                p.push(if empty_array {
                    Instruction::Array(dst, vec![])
                } else {
                    Instruction::Literal(dst, Value::Null)
                });
            },
        );
    }
    fn optional(&mut self, dst: usize, src: usize, key: &'static str) {
        self.branch(
            Test::Nullish(src),
            |p| p.push(Instruction::Mechanical(dst, "undefined", vec![])),
            |p| p.get(dst, src, key),
        );
    }
    fn boolean_result(&mut self, test: Test) {
        self.branch(test, |p| p.boolean(1, true), |p| p.boolean(1, false));
        self.push(Instruction::Return(1));
    }
    fn check(
        &mut self,
        name: &'static str,
        status: &'static str,
        evidence: usize,
        detail: &'static str,
    ) {
        self.literal(25, detail);
        self.check_detail(name, status, evidence, 25);
    }
    fn check_detail(
        &mut self,
        name: &'static str,
        status: &'static str,
        evidence: usize,
        detail: usize,
    ) {
        self.literal(26, name);
        self.literal(27, status);
        self.push(Instruction::Object(
            28,
            vec![
                ("name", 26),
                ("status", 27),
                ("detail", detail),
                ("evidence", evidence),
            ],
        ));
        self.push(Instruction::Return(28));
    }
}
fn not(test: Test) -> Test {
    Test::Not(Box::new(test))
}
fn string(text: &str) -> Value {
    Value::Str(text.encode_utf16().collect())
}
fn number(n: usize) -> Value {
    Value::Num(n as f64)
}
fn array(values: impl IntoIterator<Item = Value>) -> Value {
    Value::Arr(values.into_iter().collect())
}
fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Obj(
        fields
            .into_iter()
            .map(|(k, v)| (k.encode_utf16().collect(), v))
            .collect(),
    )
}
fn references(regs: &[Value], ids: &[usize]) -> Result<Value, Refusal> {
    ids.iter()
        .map(|&id| regs.get(id).cloned().ok_or(Refusal::Input))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Arr)
}
fn count(value: Option<&Value>) -> Result<usize, Refusal> {
    match value {
        Some(Value::Num(n))
            if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n <= u32::MAX as f64 =>
        {
            Ok(*n as usize)
        }
        _ => Err(Refusal::Input),
    }
}
fn same(value: Option<&Value>, text: &str) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| s.iter().copied().eq(text.encode_utf16()))
}
fn descriptor(value: &Value) -> bool {
    let Some(Value::Bool(_)) = value.get("truth") else {
        return false;
    };
    count(value.get("ref")).is_ok()
        && [
            "undefined",
            "null",
            "boolean",
            "number",
            "string",
            "object",
            "function",
            "symbol",
            "bigint",
        ]
        .iter()
        .any(|k| same(value.get("kind"), k))
        && (!same(value.get("kind"), "string")
            || value
                .get("prefix")
                .and_then(Value::as_str)
                .is_some_and(|s| s.len() <= 64)
                && count(value.get("length")).is_ok())
}
fn test(condition: &Test, regs: &[Value]) -> Result<bool, Refusal> {
    Ok(match condition {
        Test::Truth(r) => matches!(
            regs.get(*r).and_then(|v| v.get("truth")),
            Some(Value::Bool(true))
        ),
        Test::Kind(r, k) => same(regs.get(*r).and_then(|v| v.get("kind")), k),
        Test::Text(r, s) => regs.get(*r).is_some_and(|v| {
            same(v.get("kind"), "string")
                && same(v.get("prefix"), s)
                && count(v.get("length")).is_ok_and(|n| n == s.encode_utf16().count())
        }),
        Test::Nullish(r) => {
            same(regs.get(*r).and_then(|v| v.get("kind")), "null")
                || same(regs.get(*r).and_then(|v| v.get("kind")), "undefined")
        }
        Test::Not(inner) => !test(inner, regs)?,
    })
}
fn program(id: usize) -> Result<Program, Refusal> {
    let mut p = Program::default();
    match id {
        0 => build_program(&mut p),
        1 => tests_program(&mut p),
        2 => artifacts_program(&mut p),
        3 => plan_program(&mut p),
        4 => unresolved_program(&mut p),
        5 => checkpoint_program(&mut p),
        6 => guard_program(&mut p),
        10 => {
            p.get(1, 0, "id");
            membership(&mut p, 1, &TEST_STEP_IDS);
        }
        11 => {
            p.branch(
                Test::Truth(0),
                |p| {
                    p.get(1, 0, "kind");
                    p.branch(
                        Test::Text(1, TEST_ARTIFACT_KIND),
                        |p| {
                            p.get(1, 0, "passed");
                            p.boolean_result(Test::Kind(1, "boolean"));
                        },
                        |p| {
                            p.boolean(1, false);
                            p.push(Instruction::Return(1));
                        },
                    );
                },
                |p| p.push(Instruction::Return(0)),
            );
        }
        12 | 14 | 21 | 22 | 23 => {
            p.get(1, 0, "status");
            p.boolean_result(Test::Text(
                1,
                match id {
                    12 => "failed",
                    14 => "running",
                    21 => "fail",
                    22 => "unknown",
                    _ => "pass",
                },
            ));
        }
        13 => {
            p.get(1, 0, "passed");
            p.branch(
                Test::Kind(1, "boolean"),
                |p| p.boolean_result(not(Test::Truth(1))),
                |p| {
                    p.boolean(1, false);
                    p.push(Instruction::Return(1));
                },
            );
        }
        15 => {
            p.optional(1, 0, "name");
            p.push(Instruction::Return(1));
        }
        16 => p.boolean_result(Test::Kind(0, "string")),
        17 => {
            p.push(Instruction::Method(2, 1, "includes", vec![0], None));
            p.boolean_result(not(Test::Truth(2)));
        }
        18 => {
            p.get(1, 0, "status");
            p.boolean_result(not(Test::Text(1, "completed")));
        }
        19 => {
            p.get(1, 0, "id");
            p.push(Instruction::Template(
                3,
                vec![Part::Raw(1), Part::Text(":")],
            ));
            p.get(2, 0, "status");
            p.push(Instruction::Template(3, vec![Part::Raw(3), Part::Raw(2)]));
            p.push(Instruction::Return(3));
        }
        20 => {
            p.push(Instruction::Get(2, 1, Key::Register(0)));
            p.branch(
                Test::Kind(2, "string"),
                |p| {
                    p.push(Instruction::Get(2, 1, Key::Register(0)));
                    p.get(3, 2, "length");
                    p.push(Instruction::Mechanical(4, "greater_zero", vec![3]));
                    p.push(Instruction::Return(4));
                },
                |p| {
                    p.boolean(4, false);
                    p.push(Instruction::Return(4));
                },
            );
        }
        _ => return Err(Refusal::Input),
    }
    Ok(p)
}
fn membership(p: &mut Program, reg: usize, names: &[&'static str]) {
    if let Some((first, rest)) = names.split_first() {
        p.branch(
            Test::Text(reg, first),
            |p| {
                p.boolean(2, true);
                p.push(Instruction::Return(2));
            },
            |p| membership(p, reg, rest),
        );
    } else {
        p.boolean(2, false);
        p.push(Instruction::Return(2));
    }
}
fn build_program(p: &mut Program) {
    // Destructuring reads both fields before validating job; only undefined gets the default.
    p.branch(Test::Kind(0,"null"),|p|p.push(Instruction::ThrowType("Cannot destructure property 'job' of '(intermediate value)(intermediate value)(intermediate value)' as it is null.")),|_|{});
    p.get(1, 0, "job");
    p.get(2, 0, "expectedArtifacts");
    p.branch(
        Test::Kind(2, "undefined"),
        |p| p.push(Instruction::Literal(2, Value::Null)),
        |_| {},
    );
    p.branch(
        Test::Truth(1),
        |p| {
            p.branch(
                Test::Kind(1, "object"),
                |_| {},
                |p| {
                    p.push(Instruction::Throw(
                        "buildCompletenessReport requires a derived job",
                        400,
                    ))
                },
            )
        },
        |p| {
            p.push(Instruction::Throw(
                "buildCompletenessReport requires a derived job",
                400,
            ))
        },
    );
    for (entry, dst) in [(1, 3), (2, 4), (3, 5), (4, 6), (5, 7)] {
        p.push(Instruction::Evaluate(
            dst,
            entry,
            if entry == 2 { vec![1, 2] } else { vec![1] },
        ));
    }
    p.push(Instruction::Array(8, vec![3, 4, 5, 6, 7]));
    p.get(9, 1, "id");
    p.branch(
        Test::Nullish(9),
        |p| p.push(Instruction::Literal(9, Value::Null)),
        |_| {},
    );
    p.method(10, 8, "some", 21);
    p.branch(
        Test::Truth(10),
        |p| p.literal(11, "fail"),
        |p| {
            p.method(10, 8, "some", 22);
            p.branch(
                Test::Truth(10),
                |p| p.literal(11, "unknown"),
                |p| p.literal(11, "pass"),
            );
        },
    );
    p.push(Instruction::Object(
        12,
        vec![("jobId", 9), ("checks", 8), ("overall", 11)],
    ));
    // Existing opcode1 remains the independent refusal/representation confirmation.
    p.push(Instruction::Mechanical(13, "confirmation", vec![0, 12]));
    p.get(14, 13, "reason");
    p.branch(
        Test::Nullish(14),
        |p| p.push(Instruction::Return(12)),
        |p| {
            p.branch(
                Test::Text(11, "fail"),
                |p| p.literal(15, "fail"),
                |p| p.literal(15, "unknown"),
            );
            p.push(Instruction::Object(
                16,
                vec![
                    ("jobId", 9),
                    ("checks", 8),
                    ("overall", 15),
                    ("unverified", 14),
                ],
            ));
            p.push(Instruction::Return(16));
        },
    );
}
fn tests_program(p: &mut Program) {
    p.optional(1, 0, "steps");
    p.fallback(2, 1, true);
    p.method(3, 2, "filter", 10);
    p.optional(1, 0, "artifacts");
    p.fallback(2, 1, true);
    p.method(4, 2, "filter", 11);
    p.push(Instruction::Object(5, vec![("steps", 3), ("artifacts", 4)]));
    p.get(6, 3, "length");
    p.branch(not(Test::Truth(6)),|p|{
        p.get(6,4,"length");p.branch(not(Test::Truth(6)),|p|p.check(TESTS,"unknown",5,"No server-defined test step or server-measured test-report artifact was recorded."),|_|{});
    },|_|{});
    p.method(6, 3, "some", 12);
    p.branch(
        Test::Truth(6),
        |p| {
            p.check(
                TESTS,
                "fail",
                5,
                "A server-defined test step or test-report artifact recorded failure.",
            )
        },
        |_| {},
    );
    p.method(6, 4, "some", 13);
    p.branch(
        Test::Truth(6),
        |p| {
            p.check(
                TESTS,
                "fail",
                5,
                "A server-defined test step or test-report artifact recorded failure.",
            )
        },
        |_| {},
    );
    p.method(6, 3, "some", 14);
    p.branch(
        Test::Truth(6),
        |p| {
            p.check(
                TESTS,
                "unknown",
                5,
                "A server-defined test step has not completed yet.",
            )
        },
        |_| {},
    );
    p.check(
        TESTS,
        "pass",
        5,
        "A server-defined test step or test-report artifact reports success.",
    );
}
fn artifacts_program(p: &mut Program) {
    p.optional(2, 0, "artifacts");
    p.fallback(3, 2, true);
    p.method(4, 3, "map", 15);
    p.method(5, 4, "filter", 16);
    p.branch(
        Test::Nullish(1),
        |p| {
            p.push(Instruction::Literal(6, Value::Null));
            p.push(Instruction::Object(7, vec![("expected", 6), ("found", 5)]));
            p.check(
                ARTIFACTS,
                "unknown",
                7,
                "No expected-artifact list was declared for this job.",
            );
        },
        |_| {},
    );
    p.push(Instruction::Mechanical(6, "spread", vec![1]));
    p.push(Instruction::Method(
        7,
        6,
        "filter",
        vec![],
        Some((17, vec![5])),
    ));
    p.get(8, 7, "length");
    p.branch(
        Test::Truth(8),
        |p| {
            p.literal(8, ", ");
            p.push(Instruction::Method(9, 7, "join", vec![8], None));
            p.push(Instruction::Template(
                10,
                vec![
                    Part::Text("Missing expected artifact(s): "),
                    Part::Raw(9),
                    Part::Text("."),
                ],
            ));
            p.push(Instruction::Object(
                11,
                vec![("expected", 6), ("found", 5), ("missing", 7)],
            ));
            p.check_detail(ARTIFACTS, "fail", 11, 10);
        },
        |_| {},
    );
    p.push(Instruction::Object(11, vec![("expected", 6), ("found", 5)]));
    p.check(ARTIFACTS, "pass", 11, "All expected artifacts are present.");
}
fn plan_program(p: &mut Program) {
    p.optional(1, 0, "plan");
    p.fallback(2, 1, false);
    p.optional(1, 0, "steps");
    p.fallback(3, 1, true);
    p.push(Instruction::Object(4, vec![("plan", 2), ("steps", 3)]));
    p.branch(
        Test::Truth(2),
        |p| {
            p.get(5, 2, "status");
            p.branch(
                Test::Text(5, "skipped"),
                |p| {
                    p.check(
                        PLAN,
                        "unknown",
                        4,
                        "The plan was explicitly skipped; there are no steps to close.",
                    )
                },
                |_| {},
            );
        },
        |_| {},
    );
    p.branch(
        not(Test::Truth(2)),
        |p| {
            p.get(5, 3, "length");
            p.branch(
                not(Test::Truth(5)),
                |p| {
                    p.check(
                        PLAN,
                        "unknown",
                        4,
                        "No plan was proposed and no steps were recorded.",
                    )
                },
                |_| {},
            );
        },
        |_| {},
    );
    p.method(5, 3, "filter", 18);
    p.get(6, 5, "length");
    p.branch(
        Test::Truth(6),
        |p| {
            p.get(6, 5, "length");
            p.push(Instruction::Template(
                6,
                vec![
                    Part::Raw(6),
                    Part::Text(" step(s) are not completed (running or failed): "),
                ],
            ));
            p.method(7, 5, "map", 19);
            p.literal(8, ", ");
            p.push(Instruction::Method(9, 7, "join", vec![8], None));
            p.push(Instruction::Template(
                10,
                vec![Part::Raw(6), Part::Raw(9), Part::Text(".")],
            ));
            p.push(Instruction::Object(
                11,
                vec![("plan", 2), ("steps", 3), ("open", 5)],
            ));
            p.check_detail(PLAN, "fail", 11, 10);
        },
        |_| {},
    );
    p.get(6, 3, "length");
    p.branch(
        not(Test::Truth(6)),
        |p| {
            p.check(
                PLAN,
                "unknown",
                4,
                "A plan was proposed but no step events were recorded to verify closure.",
            )
        },
        |_| {},
    );
    p.get(6, 3, "length");
    p.push(Instruction::Template(
        7,
        vec![
            Part::Text("All "),
            Part::Raw(6),
            Part::Text(" recorded step(s) are completed."),
        ],
    ));
    p.check_detail(PLAN, "pass", 4, 7);
}
fn unresolved_program(p: &mut Program) {
    p.optional(1, 0, "uncertain");
    p.fallback(2, 1, true);
    p.optional(1, 0, "pendingApproval");
    p.fallback(3, 1, false);
    p.get(4, 2, "length");
    p.branch(Test::Truth(4), unresolved_failure, |p| {
        p.branch(Test::Truth(3), unresolved_failure, |_| {})
    });
    p.push(Instruction::Literal(4, Value::Num(0.0)));
    p.boolean(5, false);
    p.push(Instruction::Object(
        6,
        vec![("uncertainCount", 4), ("pendingApproval", 5)],
    ));
    p.check(
        UNRESOLVED,
        "pass",
        6,
        "No unresolved uncertainty or pending approval.",
    );
}
fn unresolved_failure(p: &mut Program) {
    p.get(4, 2, "length");
    p.branch(
        Test::Truth(4),
        |p| {
            p.get(4, 2, "length");
            p.push(Instruction::Template(
                5,
                vec![
                    Part::Raw(4),
                    Part::Text(" unresolved tool.uncertain event(s)"),
                ],
            ));
            p.branch(
                Test::Truth(3),
                |p| {
                    p.literal(6, "a pending approval");
                    p.push(Instruction::Array(7, vec![5, 6]));
                },
                |p| p.push(Instruction::Array(7, vec![5])),
            );
        },
        |p| {
            p.branch(
                Test::Truth(3),
                |p| {
                    p.literal(6, "a pending approval");
                    p.push(Instruction::Array(7, vec![6]));
                },
                |p| p.push(Instruction::Array(7, vec![])),
            )
        },
    );
    // Evidence is evaluated before the detail argument's join in the original check call.
    p.get(8, 2, "length");
    p.branch(
        Test::Truth(3),
        |p| p.boolean(9, true),
        |p| p.boolean(9, false),
    );
    p.push(Instruction::Object(
        10,
        vec![("uncertainCount", 8), ("pendingApproval", 9)],
    ));
    p.literal(11, " and ");
    p.push(Instruction::Method(12, 7, "join", vec![11], None));
    p.push(Instruction::Template(
        13,
        vec![Part::Raw(12), Part::Text(" remain.")],
    ));
    p.check_detail(UNRESOLVED, "fail", 10, 13);
}
fn checkpoint_program(p: &mut Program) {
    p.optional(1, 0, "checkpoint");
    p.fallback(2, 1, false);
    p.push(Instruction::Object(3, vec![("checkpoint", 2)]));
    p.branch(
        not(Test::Truth(2)),
        |p| {
            p.check(
                CHECKPOINT,
                "unknown",
                3,
                "No checkpoint was recorded for this job.",
            )
        },
        |_| {},
    );
    for (index, key) in CHECKPOINT_SHA_KEYS.iter().enumerate() {
        p.literal(4 + index, key);
    }
    p.push(Instruction::Array(10, (4..10).collect()));
    p.push(Instruction::Method(
        11,
        10,
        "find",
        vec![],
        Some((20, vec![2])),
    ));
    p.branch(
        not(Test::Truth(11)),
        |p| {
            p.check(
                CHECKPOINT,
                "unknown",
                3,
                "A checkpoint was recorded but carries no head SHA (expected until #513 lands).",
            )
        },
        |_| {},
    );
    p.push(Instruction::Get(12, 2, Key::Register(11)));
    p.push(Instruction::Mechanical(13, "string", vec![12]));
    p.push(Instruction::Object(
        14,
        vec![("checkpoint", 2), ("field", 11), ("sha", 12)],
    ));
    // Shape validation belongs to Rust; only the bounded string prefix is needed.
    p.branch(
        Test::Kind(13, "string"),
        |_| {},
        |p| p.push(Instruction::Throw("invalid string projection", 502)),
    );
    p.push(Instruction::Mechanical(15, "sha_shape", vec![13]));
    p.branch(
        Test::Truth(15),
        |p| {
            p.push(Instruction::Template(
                16,
                vec![
                    Part::Text("Checkpoint head SHA recorded ("),
                    Part::Raw(11),
                    Part::Text(")."),
                ],
            ));
            p.check_detail(CHECKPOINT, "pass", 14, 16);
        },
        |p| {
            p.push(Instruction::Template(
                16,
                vec![
                    Part::Text("Checkpoint head SHA field \""),
                    Part::Raw(11),
                    Part::Text("\" does not look like a commit SHA: "),
                ],
            ));
            p.push(Instruction::Mechanical(15, "json_stringify", vec![12]));
            p.push(Instruction::Template(
                16,
                vec![Part::Raw(16), Part::Raw(15), Part::Text(".")],
            ));
            p.check_detail(CHECKPOINT, "fail", 14, 16);
        },
    );
}
fn guard_program(p: &mut Program) {
    p.branch(
        not(Test::Truth(0)),
        |p| {
            p.boolean(9, false);
            p.push(Instruction::Return(9));
        },
        |_| {},
    );
    p.get(1, 0, "unverified");
    p.branch(
        not(Test::Kind(1, "undefined")),
        |p| {
            p.boolean(9, false);
            p.push(Instruction::Return(9));
        },
        |_| {},
    );
    p.get(1, 0, "checks");
    p.push(Instruction::Mechanical(2, "is_array", vec![1]));
    p.branch(
        not(Test::Truth(2)),
        |p| {
            p.boolean(9, false);
            p.push(Instruction::Return(9));
        },
        |_| {},
    );
    p.get(1, 0, "checks");
    p.get(2, 1, "length");
    p.branch(
        not(Test::Truth(2)),
        |p| {
            p.boolean(9, false);
            p.push(Instruction::Return(9));
        },
        |_| {},
    );
    p.get(1, 0, "checks");
    p.method(2, 1, "every", 23);
    p.push(Instruction::Return(2));
}

fn method_failure(program: usize, source: usize, name: &str) -> &'static str {
    match (program, source, name) {
        (1, 2, "filter") => "((intermediate value) || []).filter is not a function",
        (1, 3, "some") => "steps.some is not a function",
        (1, 4, "some") => "artifacts.some is not a function",
        (2, 3, "map") => "((intermediate value) || []).map is not a function",
        (2, 4, "filter") => "((intermediate value) || []).map(...).filter is not a function",
        (2, 6, "filter") => "expected.filter is not a function",
        (2, 7, "join") => "missing.join is not a function",
        (17, 1, "includes") => "found.includes is not a function",
        (3, 3, "filter") => "steps.filter is not a function",
        (3, 5, "map") => "open.map is not a function",
        (3, 7, "join") => "open.map(...).join is not a function",
        (4, 7, "join") => "parts.join is not a function",
        (5, 10, "find") => "CHECKPOINT_SHA_KEYS.find is not a function",
        (6, 1, "every") => "report.checks.every is not a function",
        (0, 8, "some") => "checks.some is not a function",
        _ => "collection method is not a function",
    }
}

pub(super) fn call(body: &[u8]) -> Result<String, Refusal> {
    if body.len() > FRAME_LIMIT {
        return Err(Refusal::TooLarge);
    }
    let frame = json::parse_utf8(body, 16).ok_or(Refusal::Input)?;
    if frame.get("version") == Some(&Value::Bool(true)) {
        return encode(&object([
            ("version", number(1)),
            ("frameLimit", number(FRAME_LIMIT)),
            ("checks", array(CHECK_NAMES.iter().map(|s| string(s)))),
            (
                "testStepIds",
                array(TEST_STEP_IDS.iter().map(|s| string(s))),
            ),
            ("testArtifactKind", string(TEST_ARTIFACT_KIND)),
        ]));
    }
    let state = frame.get("state").ok_or(Refusal::Input)?;
    let id = count(state.get("program"))?;
    let mut pc = count(state.get("pc"))?;
    let Some(Value::Arr(raw_regs)) = state.get("registers") else {
        return Err(Refusal::Input);
    };
    if raw_regs.len() > REGISTERS || !raw_regs.iter().all(|r| r == &Value::Null || descriptor(r)) {
        return Err(Refusal::Input);
    }
    let mut regs = raw_regs.clone();
    regs.resize(REGISTERS, Value::Null);
    if let Some(answer) = frame.get("answer") {
        if !descriptor(answer) {
            return Err(Refusal::Input);
        }
        let dst = count(state.get("destination"))?;
        *regs.get_mut(dst).ok_or(Refusal::Input)? = answer.clone();
    } else if state.get("destination").is_some() {
        return Err(Refusal::Input);
    }
    let program = program(id)?;
    // Only constant-size branches run here; each request advances PC without replaying history.
    for _ in 0..512 {
        let instruction = program.0.get(pc).ok_or(Refusal::Input)?;
        let (dst, request) = match instruction {
            Instruction::Jump(next) => {
                pc = *next;
                continue;
            }
            Instruction::Branch(condition, yes, no) => {
                pc = if test(condition, &regs)? { *yes } else { *no };
                continue;
            }
            Instruction::Return(src) => {
                return encode(&object([(
                    "done",
                    regs.get(*src).cloned().ok_or(Refusal::Input)?,
                )]))
            }
            Instruction::Throw(message, status) => {
                return encode(&object([(
                    "throw",
                    object([
                        ("class", string("Error")),
                        ("message", string(message)),
                        ("status", number(usize::from(*status))),
                    ]),
                )]))
            }
            Instruction::ThrowType(message) => {
                return encode(&object([(
                    "throw",
                    object([("class", string("TypeError")), ("message", string(message))]),
                )]))
            }
            Instruction::Literal(dst, v) => (
                *dst,
                object([("operation", string("literal")), ("value", v.clone())]),
            ),
            Instruction::Get(dst, src, key) => (
                *dst,
                object([
                    ("operation", string("get")),
                    ("target", regs.get(*src).cloned().ok_or(Refusal::Input)?),
                    (
                        "key",
                        match key {
                            Key::Name(k) => object([("literal", string(k))]),
                            Key::Register(r) => object([(
                                "reference",
                                regs.get(*r).cloned().ok_or(Refusal::Input)?,
                            )]),
                        },
                    ),
                ]),
            ),
            Instruction::Method(dst, src, name, args, callback) => (
                *dst,
                object([
                    ("operation", string("method")),
                    ("target", regs.get(*src).cloned().ok_or(Refusal::Input)?),
                    ("name", string(name)),
                    ("failure", string(method_failure(id, *src, name))),
                    ("arguments", references(&regs, args)?),
                    (
                        "callback",
                        match callback {
                            None => Value::Null,
                            Some((program, captures)) => object([
                                ("program", number(*program)),
                                ("captures", references(&regs, captures)?),
                            ]),
                        },
                    ),
                ]),
            ),
            Instruction::Evaluate(dst, entry, args) => (
                *dst,
                object([
                    ("operation", string("evaluate")),
                    ("program", number(*entry)),
                    ("arguments", references(&regs, args)?),
                ]),
            ),
            Instruction::Object(dst, fields) => (
                *dst,
                object([
                    ("operation", string("object")),
                    (
                        "fields",
                        array(
                            fields
                                .iter()
                                .map(|(key, r)| {
                                    Ok(array([
                                        string(key),
                                        regs.get(*r).cloned().ok_or(Refusal::Input)?,
                                    ]))
                                })
                                .collect::<Result<Vec<_>, Refusal>>()?,
                        ),
                    ),
                ]),
            ),
            Instruction::Array(dst, args) => (
                *dst,
                object([
                    ("operation", string("array")),
                    ("values", references(&regs, args)?),
                ]),
            ),
            Instruction::Template(dst, parts) => (
                *dst,
                object([
                    ("operation", string("template")),
                    (
                        "parts",
                        array(
                            parts
                                .iter()
                                .map(|part| match part {
                                    Part::Text(s) => Ok(object([("literal", string(s))])),
                                    Part::Raw(r) => Ok(object([(
                                        "reference",
                                        regs.get(*r).cloned().ok_or(Refusal::Input)?,
                                    )])),
                                })
                                .collect::<Result<Vec<_>, Refusal>>()?,
                        ),
                    ),
                ]),
            ),
            Instruction::Mechanical(dst, name, args) => {
                if *name == "sha_shape" {
                    let Some(reg) = args.first().and_then(|r| regs.get(*r)) else {
                        return Err(Refusal::Input);
                    };
                    let valid = count(reg.get("length")).is_ok_and(|n| (7..=40).contains(&n))
                        && reg
                            .get("prefix")
                            .and_then(Value::as_str)
                            .is_some_and(sha_shaped);
                    (
                        *dst,
                        object([
                            ("operation", string("literal")),
                            ("value", Value::Bool(valid)),
                        ]),
                    )
                } else {
                    (
                        *dst,
                        object([
                            ("operation", string(name)),
                            ("arguments", references(&regs, args)?),
                        ]),
                    )
                }
            }
        };
        let state = object([
            ("program", number(id)),
            ("pc", number(pc + 1)),
            ("registers", Value::Arr(regs)),
            ("destination", number(dst)),
        ]);
        return encode(&object([("request", request), ("continuation", state)]));
    }
    Err(Refusal::Input)
}
fn encode(value: &Value) -> Result<String, Refusal> {
    let mut out = Vec::new();
    tool_exchange::canonical(&mut out, value).map_err(|_| Refusal::Input)?;
    let text = String::from_utf16(&out).map_err(|_| Refusal::Input)?;
    if text.len() > FRAME_LIMIT {
        return Err(Refusal::TooLarge);
    }
    Ok(text)
}
