use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, AsArray, ListArray, ListBuilder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field};
use datafusion_common::Result;
use datafusion_expr::{ScalarFunctionArgs, ScalarUDFImpl};
use datafusion_expr_common::columnar_value::ColumnarValue;
use datafusion_expr_common::signature::{Signature, TypeSignature, Volatility};

use crate::error::generic_internal_err;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SparkSentences {
    signature: Signature,
}

impl Default for SparkSentences {
    fn default() -> Self {
        Self::new()
    }
}

impl SparkSentences {
    pub const NAME: &'static str = "sentences";

    pub fn new() -> Self {
        Self {
            signature: Signature::one_of(
                vec![TypeSignature::String(1), TypeSignature::String(3)],
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for SparkSentences {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        Self::NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::List(Arc::new(Field::new_list_field(
            DataType::List(Arc::new(Field::new_list_field(DataType::Utf8, true))),
            true,
        ))))
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let ScalarFunctionArgs { args, .. } = args;
        // We only use the first argument (str); lang and country are ignored.
        let arr = match &args[0] {
            ColumnarValue::Array(arr) => arr.clone(),
            ColumnarValue::Scalar(scalar) => scalar.to_array()?,
        };
        let result = sentences_inner(&arr)?;
        let is_scalar = matches!(&args[0], ColumnarValue::Scalar(_));
        if is_scalar {
            let scalar = datafusion_common::ScalarValue::try_from_array(&result, 0)?;
            Ok(ColumnarValue::Scalar(scalar))
        } else {
            Ok(ColumnarValue::Array(result))
        }
    }
}

fn sentences_inner(arr: &ArrayRef) -> Result<ArrayRef> {
    let string_arr = arr.as_string_opt::<i32>();
    let large_string_arr = arr.as_string_opt::<i64>();

    let inner_builder = ListBuilder::new(StringBuilder::new());
    let mut builder = ListBuilder::new(inner_builder);

    let len = arr.len();
    for i in 0..len {
        if arr.is_null(i) {
            builder.append_null();
        } else {
            let text = if let Some(a) = string_arr {
                a.value(i)
            } else if let Some(a) = large_string_arr {
                a.value(i)
            } else {
                return Err(generic_internal_err(
                    SparkSentences::NAME,
                    "Could not downcast argument to string array",
                ));
            };
            let sentences = split_into_sentences(text);
            let inner = builder.values();
            for sentence_words in &sentences {
                let word_builder = inner.values();
                for word in sentence_words {
                    word_builder.append_value(word);
                }
                inner.append(true);
            }
            builder.append(true);
        }
    }
    let array: ListArray = builder.finish();
    Ok(Arc::new(array))
}

/// Split text into sentences, then each sentence into words.
/// Approximates Java's BreakIterator behavior:
/// - Sentences are split on `.`, `!`, `?` followed by whitespace or end of string
/// - Words are sequences of Unicode letters or digits
fn split_into_sentences(text: &str) -> Vec<Vec<String>> {
    if text.is_empty() {
        return vec![];
    }

    let mut sentences = Vec::new();
    let mut current_pos = 0;
    let chars: Vec<char> = text.chars().collect();

    while current_pos < chars.len() {
        let mut end = current_pos;
        while end < chars.len() {
            if matches!(chars[end], '.' | '!' | '?') {
                end += 1;
                break;
            }
            end += 1;
        }

        let sentence: String = chars[current_pos..end].iter().collect();
        let words = extract_words(&sentence);
        if !words.is_empty() {
            sentences.push(words);
        }

        while end < chars.len() && chars[end].is_whitespace() {
            end += 1;
        }
        current_pos = end;
    }

    sentences
}

/// Extract words from a sentence.
/// Words are sequences of Unicode letters or digits.
fn extract_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current_word = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current_word.push(ch);
        } else if !current_word.is_empty() {
            words.push(std::mem::take(&mut current_word));
        }
    }
    if !current_word.is_empty() {
        words.push(current_word);
    }

    words
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use datafusion::arrow::array::StringArray;
    use datafusion_common::Result;

    use super::*;

    fn run_sentences(values: &[Option<&str>]) -> Result<ListArray> {
        let values_arr: ArrayRef = Arc::new(StringArray::from(values.to_vec()));
        let result = sentences_inner(&values_arr)?;
        Ok(result.as_any().downcast_ref::<ListArray>().unwrap().clone())
    }

    #[test]
    fn test_basic_sentences() -> Result<()> {
        let result = run_sentences(&[Some("Hi there! Good morning.")])?;
        let outer = result.value(0);
        let outer_list = outer.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(outer_list.len(), 2);

        let s1 = outer_list.value(0);
        let words1 = s1.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(words1.len(), 2);
        assert_eq!(words1.value(0), "Hi");
        assert_eq!(words1.value(1), "there");

        let s2 = outer_list.value(1);
        let words2 = s2.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(words2.len(), 2);
        assert_eq!(words2.value(0), "Good");
        assert_eq!(words2.value(1), "morning");
        Ok(())
    }

    #[test]
    fn test_single_sentence() -> Result<()> {
        let result = run_sentences(&[Some("Hello world")])?;
        let outer = result.value(0);
        let outer_list = outer.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(outer_list.len(), 1);

        let s1 = outer_list.value(0);
        let words1 = s1.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(words1.len(), 2);
        assert_eq!(words1.value(0), "Hello");
        assert_eq!(words1.value(1), "world");
        Ok(())
    }

    #[test]
    fn test_null_input() -> Result<()> {
        let result = run_sentences(&[None])?;
        assert!(result.is_null(0));
        Ok(())
    }

    #[test]
    fn test_empty_string() -> Result<()> {
        let result = run_sentences(&[Some("")])?;
        let outer = result.value(0);
        let outer_list = outer.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(outer_list.len(), 0);
        Ok(())
    }

    #[test]
    fn test_question_and_exclamation() -> Result<()> {
        let result = run_sentences(&[Some("What? Yes! OK.")])?;
        let outer = result.value(0);
        let outer_list = outer.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(outer_list.len(), 3);

        let words: Vec<Vec<String>> = (0..3)
            .map(|i| {
                let arr = outer_list.value(i);
                let w = arr.as_any().downcast_ref::<StringArray>().unwrap();
                (0..w.len()).map(|j| w.value(j).to_string()).collect()
            })
            .collect();
        assert_eq!(words[0], vec!["What"]);
        assert_eq!(words[1], vec!["Yes"]);
        assert_eq!(words[2], vec!["OK"]);
        Ok(())
    }
}
