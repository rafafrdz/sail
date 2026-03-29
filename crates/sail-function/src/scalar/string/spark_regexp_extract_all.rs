use std::any::Any;
use std::sync::Arc;

use datafusion::arrow::array::{
    Array, ArrayRef, GenericStringArray, Int64Array, ListArray, ListBuilder, OffsetSizeTrait,
    StringBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field};
use datafusion_common::utils::take_function_args;
use datafusion_common::{Result, ScalarValue};
use datafusion_expr::function::Hint;
use datafusion_expr::{ScalarFunctionArgs, ScalarUDFImpl};
use datafusion_expr_common::columnar_value::ColumnarValue;
use datafusion_expr_common::signature::{Signature, Volatility};
use regex::Regex;

use crate::error::{generic_exec_err, generic_internal_err, unsupported_data_types_exec_err};
use crate::functions_nested_utils::opt_downcast_arg;
use crate::functions_utils::make_scalar_function;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SparkRegexpExtractAll {
    signature: Signature,
}

impl Default for SparkRegexpExtractAll {
    fn default() -> Self {
        Self::new()
    }
}

impl SparkRegexpExtractAll {
    pub const NAME: &'static str = "regexp_extract_all";

    pub fn new() -> Self {
        Self {
            signature: Signature::user_defined(Volatility::Immutable),
        }
    }
}

impl ScalarUDFImpl for SparkRegexpExtractAll {
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
            DataType::Utf8,
            true,
        ))))
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        let err = || {
            Err(unsupported_data_types_exec_err(
                Self::NAME,
                "Expected (STRING, STRING) or (STRING, STRING, INT)",
                arg_types,
            ))
        };

        let mut res_types = vec![];
        for i in 0..=1 {
            res_types.push(match arg_types.get(i) {
                Some(DataType::Utf8 | DataType::Utf8View | DataType::LargeUtf8) => {
                    Ok(arg_types[i].clone())
                }
                Some(DataType::Null) => Ok(DataType::Utf8),
                _ => err(),
            });
        }
        if arg_types.len() == 3 {
            res_types.push(if arg_types[2].is_null() || arg_types[2].is_integer() {
                Ok(DataType::Int64)
            } else {
                err()
            });
        }
        res_types.into_iter().collect::<Result<Vec<_>>>()
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let ScalarFunctionArgs { mut args, .. } = args;
        if args.len() == 2 {
            args.push(ColumnarValue::Scalar(ScalarValue::Int64(Some(1))));
        }
        make_scalar_function(
            regexp_extract_all_inner,
            vec![Hint::Pad, Hint::AcceptsSingular, Hint::AcceptsSingular],
        )(&args)
    }
}

fn regexp_extract_all_inner(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args[0].data_type() {
        DataType::LargeUtf8 => regexp_extract_all_downcast::<i64>(args),
        _ => regexp_extract_all_downcast::<i32>(args),
    }
}

fn regexp_extract_all_downcast<O: OffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let [values_arr, pattern_arr, idx_arr] = take_function_args(SparkRegexpExtractAll::NAME, args)?;
    let values = values_arr.as_any().downcast_ref::<GenericStringArray<O>>();
    let pattern = pattern_arr.as_any().downcast_ref::<GenericStringArray<O>>();
    let idx = opt_downcast_arg!(idx_arr, Int64Array);

    // Also try i32 pattern if O is i64
    let pattern_i32;
    let pattern_ref: Option<&dyn StringArrayLike> = match pattern {
        Some(p) => Some(p),
        None => {
            pattern_i32 = pattern_arr
                .as_any()
                .downcast_ref::<GenericStringArray<i32>>();
            pattern_i32.map(|p| p as &dyn StringArrayLike)
        }
    };

    match (values, pattern_ref, idx.as_ref()) {
        (Some(values), Some(pattern), Some(idx)) => {
            let pattern_len = pattern.len_();
            let idx_len = idx.len();

            let pattern_scalar_opt = (pattern_len == 1 && pattern.is_valid_(0))
                .then(|| parse_regex(pattern.value_(0)))
                .transpose()?;
            let idx_scalar_opt = (idx_len == 1 && idx.is_valid(0)).then(|| idx.value(0));
            let is_pattern_null = pattern_len == 1 && pattern.is_null_(0);
            let is_idx_null = idx_len == 1 && idx.is_null(0);

            let mut builder = ListBuilder::new(StringBuilder::new());
            for i in 0..args[0].len() {
                let pattern_is_null = if pattern_len == 1 {
                    is_pattern_null
                } else {
                    pattern.is_null_(i)
                };
                let idx_is_null = if idx_len == 1 {
                    is_idx_null
                } else {
                    idx.is_null(i)
                };

                if pattern_is_null || idx_is_null || values.is_null(i) {
                    builder.append_null();
                } else {
                    let re = pattern_scalar_opt
                        .as_ref()
                        .map_or_else(|| parse_regex(pattern.value_(i)), |re| Ok(re.clone()))?;
                    let group_idx = idx_scalar_opt.unwrap_or_else(|| idx.value(i));
                    let matches = extract_all_matches(values.value(i), &re, group_idx)?;
                    builder.append_value(matches);
                }
            }
            let array: ListArray = builder.finish();
            Ok(Arc::new(array))
        }
        _ => Err(generic_internal_err(
            SparkRegexpExtractAll::NAME,
            "Could not downcast arguments to arrow arrays",
        )),
    }
}

trait StringArrayLike {
    fn len_(&self) -> usize;
    fn is_valid_(&self, i: usize) -> bool;
    fn is_null_(&self, i: usize) -> bool;
    fn value_(&self, i: usize) -> &str;
}

impl<O: OffsetSizeTrait> StringArrayLike for GenericStringArray<O> {
    fn len_(&self) -> usize {
        self.len()
    }
    fn is_valid_(&self, i: usize) -> bool {
        self.is_valid(i)
    }
    fn is_null_(&self, i: usize) -> bool {
        self.is_null(i)
    }
    fn value_(&self, i: usize) -> &str {
        self.value(i)
    }
}

fn parse_regex(pattern: &str) -> Result<Regex> {
    Regex::new(pattern)
        .map_err(|_| generic_exec_err(SparkRegexpExtractAll::NAME, "Invalid regex pattern"))
}

fn extract_all_matches(value: &str, re: &Regex, group_idx: i64) -> Result<Vec<Option<String>>> {
    if group_idx < 0 {
        return Err(generic_exec_err(
            SparkRegexpExtractAll::NAME,
            &format!("The group index must be non-negative, but got {group_idx}"),
        ));
    }
    let group_idx = group_idx as usize;
    let mut results = Vec::new();
    for caps in re.captures_iter(value) {
        let matched = caps.get(group_idx).map(|m| m.as_str().to_string());
        results.push(matched);
    }
    Ok(results)
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use datafusion::arrow::array::StringArray;
    use datafusion_common::Result;

    use super::*;

    fn run_regexp_extract_all(values: &[&str], pattern: &str, idx: i64) -> Result<ListArray> {
        let values_arr: ArrayRef = Arc::new(StringArray::from(
            values.iter().map(|s| Some(*s)).collect::<Vec<_>>(),
        ));
        let pattern_arr: ArrayRef = Arc::new(StringArray::from(vec![Some(pattern); values.len()]));
        let idx_arr: ArrayRef = Arc::new(Int64Array::from(vec![Some(idx); values.len()]));

        let result = regexp_extract_all_inner(&[values_arr, pattern_arr, idx_arr])?;
        Ok(result.as_any().downcast_ref::<ListArray>().unwrap().clone())
    }

    fn run_with_nulls(
        values: &[Option<&str>],
        patterns: &[Option<&str>],
        idxs: &[Option<i64>],
    ) -> Result<ListArray> {
        let values_arr: ArrayRef = Arc::new(StringArray::from(values.to_vec()));
        let pattern_arr: ArrayRef = Arc::new(StringArray::from(patterns.to_vec()));
        let idx_arr: ArrayRef = Arc::new(Int64Array::from(idxs.to_vec()));

        let result = regexp_extract_all_inner(&[values_arr, pattern_arr, idx_arr])?;
        Ok(result.as_any().downcast_ref::<ListArray>().unwrap().clone())
    }

    fn get_strings(result: &ListArray, row: usize) -> Vec<String> {
        let inner = result.value(row);
        let strings = inner.as_any().downcast_ref::<StringArray>().unwrap();
        (0..strings.len())
            .map(|i| strings.value(i).to_string())
            .collect()
    }

    #[test]
    fn test_basic_match() -> Result<()> {
        let result = run_regexp_extract_all(&["foo bar baz"], "(\\w+)", 1)?;
        assert_eq!(get_strings(&result, 0), vec!["foo", "bar", "baz"]);
        Ok(())
    }

    #[test]
    fn test_group_zero() -> Result<()> {
        let result = run_regexp_extract_all(&["100-200, 300-400"], "(\\d+)-(\\d+)", 0)?;
        assert_eq!(get_strings(&result, 0), vec!["100-200", "300-400"]);
        Ok(())
    }

    #[test]
    fn test_capture_groups() -> Result<()> {
        let result = run_regexp_extract_all(&["100-200, 300-400"], "(\\d+)-(\\d+)", 1)?;
        assert_eq!(get_strings(&result, 0), vec!["100", "300"]);

        let result = run_regexp_extract_all(&["100-200, 300-400"], "(\\d+)-(\\d+)", 2)?;
        assert_eq!(get_strings(&result, 0), vec!["200", "400"]);
        Ok(())
    }

    #[test]
    fn test_no_match() -> Result<()> {
        let result = run_regexp_extract_all(&["hello"], "(\\d+)", 1)?;
        assert_eq!(get_strings(&result, 0), Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn test_empty_string_input() -> Result<()> {
        let result = run_regexp_extract_all(&[""], "(\\w+)", 1)?;
        assert_eq!(get_strings(&result, 0), Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn test_null_input_value() -> Result<()> {
        let result = run_with_nulls(&[None], &[Some("(\\d+)")], &[Some(1)])?;
        assert!(result.is_null(0));
        Ok(())
    }

    #[test]
    fn test_null_pattern() -> Result<()> {
        let result = run_with_nulls(&[Some("abc")], &[None], &[Some(1)])?;
        assert!(result.is_null(0));
        Ok(())
    }

    #[test]
    fn test_null_idx() -> Result<()> {
        let result = run_with_nulls(&[Some("abc")], &[Some("(\\w+)")], &[None])?;
        assert!(result.is_null(0));
        Ok(())
    }

    #[test]
    fn test_nested_capture_groups() -> Result<()> {
        // Pattern ((a)(b)) has 3 groups: 1=ab, 2=a, 3=b
        let result = run_regexp_extract_all(&["ab ab"], "((a)(b))", 1)?;
        assert_eq!(get_strings(&result, 0), vec!["ab", "ab"]);

        let result = run_regexp_extract_all(&["ab ab"], "((a)(b))", 2)?;
        assert_eq!(get_strings(&result, 0), vec!["a", "a"]);

        let result = run_regexp_extract_all(&["ab ab"], "((a)(b))", 3)?;
        assert_eq!(get_strings(&result, 0), vec!["b", "b"]);
        Ok(())
    }

    #[test]
    fn test_no_capture_groups_idx_zero() -> Result<()> {
        // Pattern without groups, idx=0 returns entire match
        let result = run_regexp_extract_all(&["100-200,300-400"], "\\d+", 0)?;
        assert_eq!(get_strings(&result, 0), vec!["100", "200", "300", "400"]);
        Ok(())
    }

    #[test]
    fn test_optional_group_not_matched() -> Result<()> {
        // Pattern (a)?(b) — when matching "b", group 1 is not captured
        let result = run_regexp_extract_all(&["b ab"], "(a)?(b)", 1)?;
        let inner = result.value(0);
        let strings = inner.as_any().downcast_ref::<StringArray>().unwrap();
        // First match: "b" alone — group 1 didn't match → None
        assert_eq!(strings.len(), 2);
        assert!(strings.is_null(0));
        // Second match: "ab" — group 1 matched "a"
        assert_eq!(strings.value(1), "a");
        Ok(())
    }

    #[test]
    fn test_multiple_rows() -> Result<()> {
        let result = run_regexp_extract_all(&["abc 123", "def 456"], "(\\d+)", 1)?;
        assert_eq!(get_strings(&result, 0), vec!["123"]);
        assert_eq!(get_strings(&result, 1), vec!["456"]);
        Ok(())
    }

    #[test]
    fn test_unicode_input() -> Result<()> {
        let result = run_regexp_extract_all(&["cafe\u{0301} naive\u{0308}"], "(\\w+)", 1)?;
        let strings = get_strings(&result, 0);
        assert!(!strings.is_empty());
        Ok(())
    }

    #[test]
    fn test_greedy_vs_non_overlapping() -> Result<()> {
        // Regex is greedy by default; "aaa" matches once as "aaa", not ["a","a","a"]
        let result = run_regexp_extract_all(&["aaa"], "(a+)", 1)?;
        assert_eq!(get_strings(&result, 0), vec!["aaa"]);
        Ok(())
    }
}
