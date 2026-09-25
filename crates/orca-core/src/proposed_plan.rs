pub const PROPOSED_PLAN_OPEN: &str = "<proposed_plan>";
pub const PROPOSED_PLAN_CLOSE: &str = "</proposed_plan>";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposedPlanSegment {
    Agent(String),
    Plan(String),
}

#[derive(Clone, Debug, Default)]
pub struct ProposedPlanStreamParser {
    buffer: String,
    in_plan: bool,
    plan_buffer: String,
    drop_leading_plan_newline: bool,
}

impl ProposedPlanStreamParser {
    pub fn push(&mut self, delta: &str) -> Vec<ProposedPlanSegment> {
        self.buffer.push_str(delta);
        self.drain(false)
    }

    pub fn finish(&mut self) -> Vec<ProposedPlanSegment> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<ProposedPlanSegment> {
        let mut out = Vec::new();
        loop {
            if self.in_plan {
                if let Some(end) = self.buffer.find(PROPOSED_PLAN_CLOSE) {
                    let plan_and_close: String = self
                        .buffer
                        .drain(..end + PROPOSED_PLAN_CLOSE.len())
                        .collect();
                    self.plan_buffer.push_str(&plan_and_close[..end]);
                    let text = self.normalize_plan_text();
                    if !text.is_empty() {
                        out.push(ProposedPlanSegment::Plan(text));
                    }
                    self.in_plan = false;
                    self.drop_leading_plan_newline = false;
                    continue;
                }
                if finish {
                    let text = format!("{PROPOSED_PLAN_OPEN}{}{}", self.plan_buffer, self.buffer);
                    self.plan_buffer.clear();
                    self.buffer.clear();
                    self.in_plan = false;
                    self.drop_leading_plan_newline = false;
                    if !text.is_empty() {
                        out.push(ProposedPlanSegment::Agent(text));
                    }
                } else {
                    // The close tag may still be arriving: keep what could
                    // start it for the next delta.
                    let keep = pending_tag_prefix_len(&self.buffer, PROPOSED_PLAN_CLOSE);
                    let take = self.buffer.len() - keep;
                    self.plan_buffer.push_str(&self.buffer[..take]);
                    self.buffer.drain(..take);
                }
                break;
            }

            if let Some(start) = self.buffer.find(PROPOSED_PLAN_OPEN) {
                if start > 0 {
                    out.push(ProposedPlanSegment::Agent(self.buffer[..start].to_string()));
                }
                self.buffer.drain(..start + PROPOSED_PLAN_OPEN.len());
                self.in_plan = true;
                self.drop_leading_plan_newline = true;
                continue;
            }
            let keep = if finish {
                0
            } else {
                pending_tag_prefix_len(&self.buffer, PROPOSED_PLAN_OPEN)
            };
            if self.buffer.len() > keep {
                let take = self.buffer.len() - keep;
                out.push(ProposedPlanSegment::Agent(
                    self.buffer.drain(..take).collect(),
                ));
            }
            break;
        }
        out
    }

    fn normalize_plan_text(&mut self) -> String {
        let mut text = std::mem::take(&mut self.plan_buffer);
        if self.drop_leading_plan_newline {
            if let Some(stripped) = text.strip_prefix('\n') {
                text = stripped.to_string();
            }
            self.drop_leading_plan_newline = false;
        }
        text
    }
}

/// Length of the longest end of `text` that `tag` starts with.
fn pending_tag_prefix_len(text: &str, tag: &str) -> usize {
    for (index, _) in text.char_indices().rev() {
        let suffix = &text[index..];
        if suffix.len() >= tag.len() {
            break;
        }
        if tag.starts_with(suffix) {
            return suffix.len();
        }
    }
    0
}
