#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
type Error = Box<dyn std::error::Error>;
const HISTORY_CAP: usize = 450;
const MEM: &str = "MemTotal: 33554432 kB\nMemAvailable: 8388608 kB\n";

// Qualified fixture data: the original Python Random(1330) sequence's 80 inputs.
// These tuples contain only synthetic proc inputs, never copied expected answers.
const QUALIFIED_INPUTS: &[(u64, u64, u64, u64)] = &[
    (190, 63, 2727005, 1907746),
    (315, 219, 7008004, 2606014),
    (372, 141, 8478398, 4002693),
    (563, 91, 9232261, 8411319),
    (629, 518, 168037, 6811152),
    (648, 459, 2436806, 3999814),
    (825, 530, 5794527, 3708262),
    (1013, 591, 5361058, 7486764),
    (1179, 666, 5364739, 1621380),
    (1302, 1088, 565982, 1827622),
    (1432, 891, 3987133, 996751),
    (1439, 236, 670430, 1458226),
    (1451, 1055, 5416159, 969926),
    (1547, 152, 2654368, 3939738),
    (1715, 1433, 5890404, 1522789),
    (1837, 200, 6338270, 1326008),
    (1852, 671, 8805509, 4972547),
    (2039, 900, 3343410, 8640222),
    (2047, 1282, 9251316, 2659073),
    (2099, 589, 47793, 3542609),
    (2108, 1550, 7778679, 9143017),
    (2296, 722, 8049110, 6662109),
    (2335, 1064, 7090068, 6944463),
    (2507, 1438, 2055568, 2475733),
    (2693, 1075, 4686693, 4606905),
    (2837, 2382, 5132513, 1135483),
    (2886, 236, 5609205, 9947548),
    (2907, 1020, 7695845, 2523118),
    (3086, 2200, 8056187, 3984282),
    (3247, 96, 6148672, 3168130),
    (3298, 1790, 3508770, 4140014),
    (3477, 1125, 775858, 2576468),
    (3637, 241, 9552226, 338762),
    (3704, 3281, 8874001, 8889666),
    (3721, 138, 7018974, 7432872),
    (3785, 1478, 6732418, 600589),
    (3983, 3873, 8850265, 4281094),
    (4032, 133, 4874869, 729005),
    (4169, 414, 1088958, 7381899),
    (4202, 3434, 8628919, 2720692),
    (4316, 2601, 6842128, 5505208),
    (4360, 1220, 8414172, 9350830),
    (4393, 3325, 1234114, 4963636),
    (4423, 1744, 630460, 2258880),
    (4594, 119, 288261, 8868706),
    (4650, 3786, 9483546, 9409869),
    (4777, 2624, 3540061, 3200747),
    (4958, 3534, 9163764, 4046837),
    (5053, 2934, 5903370, 253176),
    (5125, 798, 1786520, 7167101),
    (5288, 776, 1321481, 3672629),
    (5347, 1647, 6147868, 6073641),
    (5440, 103, 5294977, 5372299),
    (5527, 3223, 5928672, 8739636),
    (5616, 1857, 5248542, 4089420),
    (5672, 3610, 2653077, 8137848),
    (5803, 3338, 650201, 5850396),
    (5897, 1176, 6011466, 3739181),
    (6033, 5805, 2881257, 7993123),
    (6163, 5954, 633791, 3223765),
    (6249, 4965, 1185378, 7422224),
    (6397, 6094, 6429607, 4883446),
    (6594, 4851, 5261078, 1168867),
    (6721, 4446, 9582032, 6525482),
    (6733, 3579, 7737712, 2652211),
    (6870, 2304, 9285007, 2900700),
    (7032, 6882, 4564851, 7932280),
    (7136, 398, 8171878, 4918507),
    (7274, 973, 438940, 3022628),
    (7408, 2690, 3725540, 2447966),
    (7591, 1532, 4508114, 116749),
    (7607, 7005, 7894358, 6810827),
    (7723, 3620, 9065557, 1481793),
    (7827, 7182, 6989777, 4275078),
    (7833, 6438, 2367570, 1107188),
    (8009, 4324, 2189473, 3330806),
    (8158, 2275, 667162, 3737013),
    (8278, 5421, 2682517, 3576503),
    (8436, 6052, 8848053, 3028727),
    (8484, 8478, 8201885, 4381993),
];
#[derive(Clone, Serialize)]
struct Input {
    stat: Option<String>,
    mem: Option<String>,
    ts: f64,
}
#[derive(Clone, Deserialize, Serialize)]
struct Point {
    ts: f64,
    cpu_pct: f64,
    mem_used_gb: f64,
    mem_total_gb: f64,
    mem_available_gb: f64,
}
#[derive(Serialize)]
struct Case {
    #[serde(flatten)]
    input: Input,
    point: Option<Point>,
}
#[derive(Deserialize, Serialize)]
struct Response {
    current: Option<Point>,
    history: Vec<Point>,
}
#[derive(Serialize)]
struct Retention {
    inputs: Vec<Input>,
    retained: usize,
    response: Response,
}
#[derive(Serialize)]
struct Fixture {
    hw_sha256: String,
    api_sha256: String,
    cases: Vec<Case>,
    samples_response: Response,
    retention: Retention,
}
#[derive(Deserialize)]
struct Sample {
    point: Option<Point>,
    last_cpu: Option<(i64, i64)>,
}

// Only actual-source AST loading/evaluation and fixture I/O. All cases, CPU-state
// handoffs, ring retention, source hashes and final fixture serialization are Rust-owned.
const LOADER: &str = r#"
import ast, dataclasses, io, json, pathlib, types, sys
p=pathlib.Path(sys.argv[1])
def load(name,names,space):
    nodes=[n for n in ast.parse((p/name).read_bytes()).body if isinstance(n,(ast.FunctionDef,ast.ClassDef)) and n.name in names]
    for n in nodes:
        if isinstance(n,ast.FunctionDef): n.decorator_list=[]
    exec(compile(ast.Module(body=nodes,type_ignores=[]),str(p/name),'exec'),space)
space={'dataclass':dataclasses.dataclass}
load('hw.py',{'HostPoint','_read_host'},space)
api={'Any':object,'Path':pathlib.Path,'dataclasses':dataclasses}
load('api.py',{'_plain','host'},api)
for line in sys.stdin:
    request=json.loads(line)
    if request['operation']=='sample':
        def op(path,**kwargs):
            value=request['input']['stat' if path=='/proc/stat' else 'mem']
            if value is None: raise OSError('synthetic failure')
            return io.StringIO(value)
        space.update(open=op,time=types.SimpleNamespace(time=lambda:request['input']['ts']),_last_cpu=request['last_cpu'])
        point=space['_read_host']()
        answer={'point':dataclasses.asdict(point) if point else None,'last_cpu':space['_last_cpu']}
    else:
        api['hw']=types.SimpleNamespace(host_history=lambda:[space['HostPoint'](**point) for point in request['history']])
        answer=api['host']()
    print(json.dumps(answer),flush=True)
"#;
struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Drop for Oracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Oracle {
    fn start(source: &Path) -> Result<Self, Error> {
        let mut child = Command::new("python3")
            .args(["-c", LOADER])
            .arg(source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().ok_or("missing oracle stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("missing oracle stdout")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }
    fn evaluate<T: serde::de::DeserializeOwned>(
        &mut self,
        request: serde_json::Value,
    ) -> Result<T, Error> {
        serde_json::to_writer(&mut self.input, &request)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let mut response = String::new();
        self.output.read_line(&mut response)?;
        if response.is_empty() || response.len() > 1024 * 1024 {
            return Err("invalid oracle frame".into());
        }
        Ok(serde_json::from_str(&response)?)
    }
}
#[derive(Default)]
struct Sequence {
    last_cpu: Option<(i64, i64)>,
    history: VecDeque<Point>,
}
impl Sequence {
    fn sample(&mut self, oracle: &mut Oracle, input: Input) -> Result<Case, Error> {
        let answer: Sample = oracle.evaluate(
            serde_json::json!({"operation":"sample","input":input,"last_cpu":self.last_cpu}),
        )?;
        self.last_cpu = answer.last_cpu;
        if let Some(point) = &answer.point {
            if self.history.len() == HISTORY_CAP {
                self.history.pop_front();
            }
            self.history.push_back(point.clone());
        }
        Ok(Case {
            input,
            point: answer.point,
        })
    }
    fn response(&self, oracle: &mut Oracle) -> Result<Response, Error> {
        oracle.evaluate(serde_json::json!({"operation":"host","history":self.history}))
    }
}
fn inputs() -> Vec<Input> {
    let fixed = [
        (Some("cpu 1 2 3 40 5\ncpu0 ignored\n"), Some(MEM)),
        (Some("cpu 11 2 3 45 5\n"), Some(MEM)),
        (Some("cpu 21 2 3 45 5\n"), None),
        (Some("cpu 31 2 3 55 5\n"), Some(MEM)),
        (None, Some(MEM)),
        (Some("cpu bad 0 0 1\n"), Some(MEM)),
        (Some("cpu 1 2 3\n"), Some(MEM)),
        (Some("cpu 31 2 3 55 5\n"), Some(MEM)),
        (Some("cpu 0 0 0 0\n"), Some("MemAvailable: 1048576 kB\n")),
        (Some("cpu 0 0 0 0\n"), Some("")),
        (Some("cpu 1 0 0 0\n"), Some("MemTotal: bad kB\n")),
        (
            Some("cpu 2 0 0 0\n"),
            Some("MemTotal: 131072 kB\nMemAvailable: 262144 kB\n"),
        ),
        (
            Some("cpu 3 0 0 0\n"),
            Some("MemTotal: 123456 kB\nMemTotal: 524288 kB\n"),
        ),
    ];
    let mut out: Vec<Input> = fixed
        .into_iter()
        .enumerate()
        .map(|(i, (stat, mem))| Input {
            stat: stat.map(str::to_owned),
            mem: mem.map(str::to_owned),
            ts: 1000.0 + i as f64,
        })
        .collect();
    out.extend(
        QUALIFIED_INPUTS
            .iter()
            .enumerate()
            .map(|(i, (cpu, idle, total, available))| Input {
                stat: Some(format!("cpu {cpu} 0 0 {idle} 0\n")),
                mem: Some(format!(
                    "MemTotal: {total} kB\nMemAvailable: {available} kB\n"
                )),
                ts: 1100.0 + i as f64,
            }),
    );
    out
}
fn main() -> Result<(), Error> {
    let source = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: generate-host-oracle SERVICES/app")?,
    );
    let hw_sha256 = format!("{:x}", Sha256::digest(std::fs::read(source.join("hw.py"))?));
    let api_sha256 = format!(
        "{:x}",
        Sha256::digest(std::fs::read(source.join("api.py"))?)
    );
    let mut oracle = Oracle::start(&source)?;
    let mut sequence = Sequence::default();
    let mut cases = Vec::new();
    for input in inputs() {
        cases.push(sequence.sample(&mut oracle, input)?);
    }
    let samples_response = sequence.response(&mut oracle)?;
    let mut sequence = Sequence::default();
    let mut inputs = Vec::new();
    for i in 0..460 {
        let input = Input {
            stat: Some(format!("cpu {i} 0 0 {i}\n")),
            mem: Some(MEM.into()),
            ts: 2000.0 + f64::from(i),
        };
        sequence.sample(&mut oracle, input.clone())?;
        inputs.push(input);
    }
    let retention = Retention {
        inputs,
        retained: sequence.history.len(),
        response: sequence.response(&mut oracle)?,
    };
    let fixture = Fixture {
        hw_sha256,
        api_sha256,
        cases,
        samples_response,
        retention,
    };
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &fixture)?;
    stdout.write_all(b"\n")?;
    Ok(())
}
