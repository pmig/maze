//! The index schema (PLAN §4.4 minus the mutable fields) and its analyzers.

use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language as StemLanguage, LowerCaser, RawTokenizer, RemoveLongFilter,
    SimpleTokenizer, Stemmer, TextAnalyzer,
};

use tantivy::Index;

/// Lowercase + ASCII folding, no stemming: every message, every language.
pub(crate) const NEUTRAL: &str = "mazemail_neutral";
/// Neutral plus the German Snowball stemmer.
pub(crate) const GERMAN: &str = "mazemail_de";
/// Neutral plus the English Snowball stemmer.
pub(crate) const ENGLISH: &str = "mazemail_en";
/// The whole value, lowercased: addresses and list ids for facets.
pub(crate) const RAW_LOWER: &str = "mazemail_raw_lower";

/// Tokens longer than this (base64 debris, hashes) are dropped.
const MAX_TOKEN_LEN: usize = 48;

/// Handles to every field.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fields {
    pub(crate) id: Field,
    pub(crate) thread: Field,
    pub(crate) account: Field,
    pub(crate) date: Field,
    pub(crate) size: Field,
    pub(crate) has_attachment: Field,
    pub(crate) from: Field,
    pub(crate) to: Field,
    pub(crate) cc: Field,
    pub(crate) from_addr: Field,
    pub(crate) to_addr: Field,
    pub(crate) cc_addr: Field,
    pub(crate) subject: Field,
    pub(crate) body: Field,
    pub(crate) subject_de: Field,
    pub(crate) body_de: Field,
    pub(crate) subject_en: Field,
    pub(crate) body_en: Field,
    pub(crate) attachment_names: Field,
    pub(crate) attachment_text: Field,
    pub(crate) list_id: Field,
    pub(crate) list_id_raw: Field,
}

/// Field names used with Tantivy's by-name APIs (fast fields, aggregations).
pub(crate) mod names {
    pub(crate) const ID: &str = "id";
    pub(crate) const THREAD: &str = "thread";
    pub(crate) const DATE: &str = "date";
    pub(crate) const HAS_ATTACHMENT: &str = "has_attachment";
    pub(crate) const FROM_ADDR: &str = "from_addr";
    pub(crate) const TO_ADDR: &str = "to_addr";
    pub(crate) const CC_ADDR: &str = "cc_addr";
}

/// Text with positions, for phrase queries.
fn positional(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// Text with term frequencies only. The stemmed copies don't need positions: phrases
/// are exact and only run against the neutral fields.
fn frequencies(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqs),
    )
}

/// A columnar (fast) string, not searchable: facets only.
fn facet_string() -> TextOptions {
    TextOptions::default().set_fast(RAW_LOWER)
}

/// Builds the schema. Nothing is stored: hits are resolved through fast fields.
pub(crate) fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let fields = Fields {
        id: builder.add_u64_field(names::ID, INDEXED | FAST),
        thread: builder.add_u64_field(names::THREAD, FAST),
        account: builder.add_u64_field("account", INDEXED | FAST),
        date: builder.add_i64_field(names::DATE, FAST),
        size: builder.add_u64_field("size", FAST),
        has_attachment: builder.add_bool_field(names::HAS_ATTACHMENT, INDEXED | FAST),
        from: builder.add_text_field("from", positional(NEUTRAL)),
        to: builder.add_text_field("to", positional(NEUTRAL)),
        cc: builder.add_text_field("cc", positional(NEUTRAL)),
        from_addr: builder.add_text_field(names::FROM_ADDR, facet_string()),
        to_addr: builder.add_text_field(names::TO_ADDR, facet_string()),
        cc_addr: builder.add_text_field(names::CC_ADDR, facet_string()),
        subject: builder.add_text_field("subject", positional(NEUTRAL)),
        body: builder.add_text_field("body", positional(NEUTRAL)),
        subject_de: builder.add_text_field("subject_de", frequencies(GERMAN)),
        body_de: builder.add_text_field("body_de", frequencies(GERMAN)),
        subject_en: builder.add_text_field("subject_en", frequencies(ENGLISH)),
        body_en: builder.add_text_field("body_en", frequencies(ENGLISH)),
        attachment_names: builder.add_text_field("attachment_names", positional(NEUTRAL)),
        attachment_text: builder.add_text_field("attachment_text", positional(NEUTRAL)),
        list_id: builder.add_text_field("list_id", positional(NEUTRAL)),
        list_id_raw: builder.add_text_field("list_id_raw", facet_string()),
    };
    (builder.build(), fields)
}

pub(crate) fn neutral() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_LEN))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

/// Stemming runs before folding: the Snowball stemmers expect umlauts and `ß`.
pub(crate) fn stemmed(language: StemLanguage) -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_LEN))
        .filter(LowerCaser)
        .filter(Stemmer::new(language))
        .filter(AsciiFoldingFilter)
        .build()
}

fn raw_lower() -> TextAnalyzer {
    TextAnalyzer::builder(RawTokenizer::default())
        .filter(LowerCaser)
        .build()
}

/// Analyzers live in memory only, so every opened index needs them registered.
pub(crate) fn register_tokenizers(index: &Index) {
    let tokenizers = index.tokenizers();
    tokenizers.register(NEUTRAL, neutral());
    tokenizers.register(GERMAN, stemmed(StemLanguage::German));
    tokenizers.register(ENGLISH, stemmed(StemLanguage::English));
    tokenizers.register(RAW_LOWER, raw_lower());
    index
        .fast_field_tokenizer()
        .register(RAW_LOWER, raw_lower());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(mut analyzer: TextAnalyzer, text: &str) -> Vec<String> {
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    #[test]
    fn neutral_folds_without_stemming() {
        assert_eq!(
            tokens(neutral(), "Grüße an Anna, anna@Example.com: Rechnungen"),
            [
                "grusse",
                "an",
                "anna",
                "anna",
                "example",
                "com",
                "rechnungen"
            ]
        );
    }

    #[test]
    fn stemmers_reduce_plurals() {
        assert_eq!(
            tokens(stemmed(StemLanguage::German), "Rechnungen Rechnung Häuser"),
            ["rechnung", "rechnung", "haus"]
        );
        assert_eq!(
            tokens(stemmed(StemLanguage::English), "invoices invoice running"),
            ["invoic", "invoic", "run"]
        );
    }

    #[test]
    fn long_tokens_are_dropped() {
        let long = "a".repeat(100);
        assert_eq!(tokens(neutral(), &format!("x {long} y")), ["x", "y"]);
    }
}
