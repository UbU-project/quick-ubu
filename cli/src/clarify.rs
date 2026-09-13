//! Task clarification engine and replaceable answer collection.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionKind {
    YesNo,
    ShortText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub text: String,
    pub kind: QuestionKind,
    /// Only relevant if question `0` was answered with `1` (case-insensitive).
    pub depends_on: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarifyResponse {
    pub questions: Vec<Question>,
    pub tags: Vec<String>,
    pub done: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Answers {
    pub answers: BTreeMap<String, String>,
    pub stop: bool,
}

pub trait AnswerCollector {
    fn collect(&mut self, questions: &[Question]) -> Answers;
}
