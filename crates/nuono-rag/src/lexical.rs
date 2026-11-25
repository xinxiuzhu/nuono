use std::collections::BTreeMap;

use crate::{RagError, Result};

/// 词法检索配额：同时限制原始输入文本的字节数和倒排索引的词条记录数。
/// 分词总数与倒排记录上限可防止中日韩单字/双字切分（1/2-gram）消耗过多算力与内存。
#[derive(Clone, Copy, Debug)]
pub struct LexicalBudget {
    pub max_rows: usize,
    pub max_document_bytes: usize,
    pub max_corpus_bytes: usize,
    pub max_query_bytes: usize,
    pub max_tokens: usize,
    pub max_term_occurrences: usize,
    pub max_query_tokens: usize,
}

impl Default for LexicalBudget {
    fn default() -> Self {
        Self {
            max_rows: 4096,
            max_document_bytes: 16 * 1024,
            max_corpus_bytes: 8 * 1024 * 1024,
            max_query_bytes: 4096,
            max_tokens: 1024 * 1024,
            max_term_occurrences: 256 * 1024,
            max_query_tokens: 2048,
        }
    }
}

/// 固定在 `0.0..=1.0` 量纲上的关键词匹配得分，不随当前批次最高分做相对归一化。
/// 计算公式为 `relevance = coverage * bm25 / (1 + bm25)`，因此孤立的弱匹配不会因为其他行都是 0 分就被放大成满分。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LexicalScore {
    pub bm25: f64,
    pub coverage: f64,
    pub relevance: f64,
    pub matched_terms: usize,
    pub query_terms: usize,
    /// 命中的精确词条数（完整的英文单词/数字，或中日韩双字词组；单字查询时该单字也算作精确词条）。
    pub matched_exact_terms: usize,
    pub query_exact_terms: usize,
}

#[derive(Debug)]
struct Posting {
    row: usize,
    frequency: usize,
}

#[derive(Debug)]
struct Term {
    frequency: usize,
    exact: bool,
}

/// 基于文本行的内存 BM25 倒排索引。调用方需在建索引前自行维护行 ID 并完成作用域、权限与有效状态过滤。
/// 索引内部不保存原始文本，也不涉及任何网络、数据库或全局状态。
#[derive(Debug)]
pub struct Bm25Index {
    postings: BTreeMap<String, Vec<Posting>>,
    lengths: Vec<usize>,
    average_length: f64,
    budget: LexicalBudget,
}

impl Bm25Index {
    pub fn new<T: AsRef<str>>(texts: &[T]) -> Result<Self> {
        Self::with_budget(texts, LexicalBudget::default())
    }

    pub fn with_budget<T: AsRef<str>>(texts: &[T], budget: LexicalBudget) -> Result<Self> {
        if texts.len() > budget.max_rows
            || budget.max_rows == 0
            || budget.max_document_bytes == 0
            || budget.max_corpus_bytes == 0
            || budget.max_query_bytes == 0
            || budget.max_tokens == 0
            || budget.max_term_occurrences == 0
            || budget.max_query_tokens == 0
        {
            return Err(RagError::LexicalBudget);
        }
        // 在构建倒排索引前，先校验所有文本的合法性与字节配额。
        let mut corpus_bytes = 0usize;
        for text in texts {
            let text = text.as_ref();
            if text.contains('\0') {
                return Err(RagError::InvalidText);
            }
            corpus_bytes = corpus_bytes
                .checked_add(text.len())
                .filter(|bytes| *bytes <= budget.max_corpus_bytes)
                .ok_or(RagError::LexicalBudget)?;
            if text.len() > budget.max_document_bytes {
                return Err(RagError::LexicalBudget);
            }
        }
        let mut postings = BTreeMap::<String, Vec<Posting>>::new();
        let mut lengths = Vec::with_capacity(texts.len());
        let mut total_tokens = 0usize;
        let mut term_occurrences = 0usize;
        for (row, text) in texts.iter().enumerate() {
            let remaining_tokens = budget.max_tokens - total_tokens;
            let (terms, length) =
                tokenize(text.as_ref(), remaining_tokens).ok_or(RagError::LexicalBudget)?;
            total_tokens += length;
            term_occurrences = term_occurrences
                .checked_add(terms.len())
                .filter(|count| *count <= budget.max_term_occurrences)
                .ok_or(RagError::LexicalBudget)?;
            lengths.push(length);
            for (term, data) in terms {
                postings.entry(term).or_default().push(Posting {
                    row,
                    frequency: data.frequency,
                });
            }
        }
        let average_length = if texts.is_empty() || total_tokens == 0 {
            1.0
        } else {
            total_tokens as f64 / texts.len() as f64
        };
        Ok(Self {
            postings,
            lengths,
            average_length,
            budget,
        })
    }

    pub fn len(&self) -> usize {
        self.lengths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    /// 返回与输入行顺序一一对应的评分列表。空查询、纯标点或纯停用词查询均返回 0 分；
    /// 超长或包含 NUL 字符的查询直接返回错误。
    pub fn scores(&self, query: &str) -> Result<Vec<LexicalScore>> {
        if query.contains('\0') {
            return Err(RagError::InvalidText);
        }
        if query.len() > self.budget.max_query_bytes {
            return Err(RagError::LexicalQueryBudget);
        }
        let (terms, _) =
            tokenize(query, self.budget.max_query_tokens).ok_or(RagError::LexicalQueryBudget)?;
        let query_terms = terms.len();
        let query_exact_terms = terms
            .values()
            .filter(|term| term.exact || query_terms == 1)
            .count();
        let mut scores = vec![
            LexicalScore {
                query_terms,
                query_exact_terms,
                ..LexicalScore::default()
            };
            self.len()
        ];
        if self.is_empty() || terms.is_empty() {
            return Ok(scores);
        }
        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        for (term, query_term) in &terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let df = postings.len() as f64;
            let idf = (1.0 + (self.len() as f64 - df + 0.5) / (df + 0.5)).ln();
            for posting in postings {
                let tf = posting.frequency as f64;
                let length = self.lengths[posting.row] as f64;
                let denominator = tf + K1 * (1.0 - B + B * length / self.average_length);
                let score = &mut scores[posting.row];
                score.bm25 += idf * tf * (K1 + 1.0) / denominator;
                score.matched_terms += 1;
                if query_term.exact || query_terms == 1 {
                    score.matched_exact_terms += 1;
                }
            }
        }
        for score in &mut scores {
            score.coverage = score.matched_terms as f64 / query_terms as f64;
            score.relevance = score.coverage * (score.bm25 / (1.0 + score.bm25));
            if !score.bm25.is_finite() || !score.relevance.is_finite() {
                return Err(RagError::InvalidHybridScore);
            }
        }
        Ok(scores)
    }
}

/// 英文及其他字母文字按完整小写单词切分，数字保持为完整词条；
/// 中日韩连续文本同时切分为单字（1-gram）和相邻双字（2-gram），不依赖外部词典或分词器。
fn tokenize(text: &str, max_tokens: usize) -> Option<(BTreeMap<String, Term>, usize)> {
    let mut terms = BTreeMap::<String, Term>::new();
    let mut count = 0usize;
    let mut word = String::new();
    let mut previous_cjk = None;
    let mut emit = |token: String, exact: bool| {
        if is_stop_word(&token) {
            return Some(());
        }
        count = count.checked_add(1).filter(|count| *count <= max_tokens)?;
        let term = terms.entry(token).or_insert(Term {
            frequency: 0,
            exact,
        });
        term.frequency += 1;
        term.exact |= exact;
        Some(())
    };
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        // 在切分单字/双字前先整词跳过常见的中文疑问虚词；
        // 若只在切分后过滤双字词“请问”，会残留孤立的单字“问”干扰检索。
        if let Some(phrase) = CJK_STOP_PHRASES
            .iter()
            .find(|phrase| rest.starts_with(**phrase))
        {
            if !word.is_empty() {
                emit(std::mem::take(&mut word), true)?;
            }
            previous_cjk = None;
            offset += phrase.len();
            continue;
        }
        let ch = rest.chars().next()?;
        offset += ch.len_utf8();
        if is_cjk(ch) {
            if !word.is_empty() {
                emit(std::mem::take(&mut word), true)?;
            }
            emit(ch.to_string(), false)?;
            if let Some(previous) = previous_cjk {
                emit(format!("{previous}{ch}"), true)?;
            }
            previous_cjk = Some(ch);
        } else if ch.is_alphanumeric() {
            previous_cjk = None;
            word.extend(ch.to_lowercase());
        } else {
            previous_cjk = None;
            if !word.is_empty() {
                emit(std::mem::take(&mut word), true)?;
            }
        }
    }
    if !word.is_empty() {
        emit(word, true)?;
    }
    Some((terms, count))
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff |
        0x20000..=0x2a6df | 0x2a700..=0x2ebef | 0x30000..=0x323af
    )
}

const CJK_STOP_PHRASES: &[&str] = &[
    "什么", "怎么", "如何", "是否", "这个", "那个", "哪些", "哪里", "现在", "今天", "一下", "请问",
    "知道", "时候",
];

// 精简的停用词表，防止常见虚词和提问套话拉高无关文档的匹配度（不会修改调用方的原始文本）。
fn is_stop_word(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "of"
            | "to"
            | "in"
            | "on"
            | "at"
            | "for"
            | "with"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "i"
            | "me"
            | "my"
            | "you"
            | "your"
            | "it"
            | "its"
            | "we"
            | "our"
            | "they"
            | "their"
            | "do"
            | "does"
            | "did"
            | "have"
            | "has"
            | "had"
            | "what"
            | "which"
            | "how"
            | "when"
            | "where"
            | "please"
            | "can"
            | "could"
            | "would"
            | "should"
            | "的"
            | "我"
            | "你"
            | "他"
            | "她"
            | "它"
            | "们"
            | "是"
            | "了"
            | "吗"
            | "呢"
            | "啊"
            | "和"
            | "与"
            | "及"
            | "这"
            | "那"
            | "请"
            | "什么"
            | "怎么"
            | "如何"
            | "是否"
            | "这个"
            | "那个"
            | "哪些"
            | "哪里"
            | "现在"
            | "今天"
            | "一下"
            | "请问"
            | "知道"
            | "时候"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_ngrams_english_words_and_numbers_are_exact_and_case_insensitive() {
        let texts = [
            "林小满的电话号码末四位是 3142；喜欢 EarlGrey。",
            "林小雨的电话号码末四位是 2718；喜欢 coffee。",
            "我参加了 13142 号活动。",
        ];
        let index = Bm25Index::new(&texts).unwrap();
        let scores = index.scores("林小满 3142 earlgrey").unwrap();
        assert_eq!(scores[0].coverage, 1.0);
        assert!(scores[0].relevance > scores[1].relevance);
        assert_eq!(scores[2].bm25, 0.0, "numbers must not match a substring");
        assert_eq!(index.scores("EARLGREY").unwrap()[0].coverage, 1.0);
        assert_eq!(
            index.scores("EARLGREY EarlGrey").unwrap(),
            index.scores("earlgrey").unwrap(),
            "repeating query terms must not inflate relevance"
        );
        assert_eq!(index.scores("earl").unwrap()[0].bm25, 0.0);
        assert_eq!(index.scores("满").unwrap()[0].matched_exact_terms, 1);
    }

    #[test]
    fn fixed_score_scale_does_not_promote_the_only_weak_match() {
        let index = Bm25Index::new(&["青岛 海边", "项目预算已经审批"]).unwrap();
        let scores = index
            .scores("青岛 成都 上海 西安 深圳 广州 天津 宁波")
            .unwrap();
        assert!(scores[0].bm25 > 0.0);
        assert!(scores[0].relevance < 0.15);
        assert_eq!(scores[1].relevance, 0.0);
        for query in ["", " ! ? ", "what is the", "请问", "今天如何", "现在什么"] {
            assert!(
                index
                    .scores(query)
                    .unwrap()
                    .iter()
                    .all(|score| score.bm25 == 0.0 && score.query_terms == 0)
            );
        }
    }

    #[test]
    fn document_query_token_and_posting_limits_are_checked() {
        let text = ["甲乙 丙丁"];
        assert!(matches!(
            Bm25Index::with_budget(
                &["甲", "乙"],
                LexicalBudget {
                    max_rows: 1,
                    ..Default::default()
                }
            ),
            Err(RagError::LexicalBudget)
        ));
        for budget in [
            LexicalBudget {
                max_document_bytes: 1,
                ..Default::default()
            },
            LexicalBudget {
                max_corpus_bytes: 1,
                ..Default::default()
            },
            LexicalBudget {
                max_tokens: 1,
                ..Default::default()
            },
            LexicalBudget {
                max_term_occurrences: 1,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                Bm25Index::with_budget(&text, budget),
                Err(RagError::LexicalBudget)
            ));
        }
        assert!(matches!(
            Bm25Index::new(&["a\0b"]),
            Err(RagError::InvalidText)
        ));
        let index = Bm25Index::with_budget(
            &text,
            LexicalBudget {
                max_query_bytes: 6,
                max_query_tokens: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            index.scores("甲乙丙"),
            Err(RagError::LexicalQueryBudget)
        ));
        assert!(matches!(
            index.scores("甲乙"),
            Err(RagError::LexicalQueryBudget)
        ));
        assert!(matches!(index.scores("\0"), Err(RagError::InvalidText)));
        let empty = Bm25Index::new::<&str>(&[]).unwrap();
        assert!(empty.is_empty());
        assert!(empty.scores("test").unwrap().is_empty());
    }
}
