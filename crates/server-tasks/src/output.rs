//! The executor's bounded display stream. Thoughts never enter this accumulator. UTF16 halves
//! split across updates are joined before UTF8 byte budgets are measured, as in the old harness.
use js_json::{wtf16, JValue};
#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Limit,
    Event(E),
}
#[derive(Default)]
pub struct Output {
    bytes: usize,
    buffer: String,
    buffer_bytes: usize,
    truncated: bool,
    pending_high: Option<u16>,
    truncation_recorded: bool,
    events: usize,
    early_flushes: usize,
    break_after_tool: bool,
}
impl Output {
    fn emit<E>(&mut self, emit: &mut impl FnMut(JValue) -> Result<(), E>) -> Result<(), Error<E>> {
        if self.buffer.is_empty() && (!self.truncated || self.truncation_recorded) {
            return Ok(());
        }
        if self.events >= 64 {
            return Err(Error::Limit);
        }
        let data = JValue::Obj(vec![
            ("text".into(), JValue::Str(self.buffer.clone())),
            (
                "truncated".into(),
                JValue::Bool(self.truncated && !self.truncation_recorded),
            ),
        ]);
        emit(data).map_err(Error::Event)?;
        self.events += 1;
        self.buffer.clear();
        self.buffer_bytes = 0;
        if self.truncated {
            self.truncation_recorded = true;
        }
        Ok(())
    }
    fn append<E>(
        &mut self,
        text: &str,
        final_chunk: bool,
        emit: &mut impl FnMut(JValue) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        if self.truncated {
            return Ok(());
        }
        let mut units = Vec::new();
        if let Some(high) = self.pending_high.take() {
            units.push(high);
        }
        for c in text.chars() {
            if let Some(unit) = wtf16::lone_unit(c) {
                units.push(unit);
            } else {
                let mut pair = [0; 2];
                units.extend_from_slice(c.encode_utf16(&mut pair));
            }
        }
        if !final_chunk && units.last().is_some_and(|u| (0xd800..=0xdbff).contains(u)) {
            self.pending_high = units.pop();
        }
        for decoded in char::decode_utf16(units) {
            let c = decoded.unwrap_or('\u{fffd}');
            let bytes = c.len_utf8();
            if self.bytes + bytes > 32768 {
                self.truncated = true;
                self.emit(emit)?;
                break;
            }
            if self.buffer_bytes + bytes > 1024 {
                self.emit(emit)?;
            }
            self.buffer.push(c);
            self.buffer_bytes += bytes;
            self.bytes += bytes;
            if self.buffer_bytes == 1024 {
                self.emit(emit)?;
            }
        }
        Ok(())
    }
    pub fn message<E>(
        &mut self,
        text: &str,
        emit: &mut impl FnMut(JValue) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        if text.is_empty() {
            return Ok(());
        }
        if self.break_after_tool {
            self.append("\n\n", false, emit)?;
        }
        self.break_after_tool = false;
        self.append(text, false, emit)
    }
    pub fn tool<E>(
        &mut self,
        announcement: bool,
        emit: &mut impl FnMut(JValue) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        if announcement
            && (!self.buffer.is_empty() || self.pending_high.is_some())
            && self.early_flushes < 30
        {
            self.append("", true, emit)?;
            self.emit(emit)?;
            self.early_flushes += 1;
        }
        if self.bytes > 0 {
            self.break_after_tool = true;
        }
        Ok(())
    }
    pub fn flush<E>(
        &mut self,
        emit: &mut impl FnMut(JValue) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        self.append("", true, emit)?;
        self.emit(emit)
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn native_executor_stream_matches_node_byte_for_byte() {
        let fixture = wtf16::parse(include_str!("../tests/fixtures/output-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        assert_eq!(cases.len(), 7);
        for (index, case) in cases.iter().enumerate() {
            let mut output = Output::default();
            let mut events = Vec::new();
            let mut emit = |data| {
                events.push(data);
                Ok::<(), std::convert::Infallible>(())
            };
            let JValue::Arr(actions) = case.get("actions") else {
                panic!()
            };
            for action in actions {
                match action.get("kind").as_str().unwrap() {
                    "message" => output
                        .message(action.get("text").as_str().unwrap(), &mut emit)
                        .unwrap(),
                    kind => output.tool(kind == "tool", &mut emit).unwrap(),
                }
            }
            output.flush(&mut emit).unwrap();
            assert_eq!(JValue::Arr(events), *case.get("expected"), "case {index}");
        }
    }
}
