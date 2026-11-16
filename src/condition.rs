//! The SQLSTATE each PL/pgSQL condition name stands for, as PostgreSQL 18's
//! `plerrcodes.h` lists them.
//!
//! ```
//! use sqlparser_plpgsql::condition::{ConditionName, errcode};
//! use sqlstate::pg;
//!
//! let name: ConditionName = "null_value_not_allowed".parse()?;
//! assert_eq!(name.sqlstate(), pg::NULL_VALUE_NOT_ALLOWED);
//! assert_eq!(
//!     name.sqlstates().collect::<Vec<_>>(),
//!     [pg::NULL_VALUE_NOT_ALLOWED, pg::E_R_I_E_NULL_VALUE_NOT_ALLOWED],
//! );
//! assert_eq!(errcode("check_violation")?, Some(pg::CHECK_VIOLATION));
//! assert_eq!(errcode("P0001")?, Some(pg::RAISE_EXCEPTION));
//! assert!(errcode("Check_Violation").is_err());
//! # Ok::<(), sqlparser_plpgsql::condition::UnknownCondition>(())
//! ```

use alloc::string::String;
use core::{fmt, str::FromStr};

use sqlstate::{SqlState, pg};

/// A condition name PL/pgSQL knows, such as `unique_violation`.
///
/// A handler's `others` is not one, since it stands for no SQLSTATE.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConditionName(&'static (&'static str, SqlState));

impl ConditionName {
    /// The name, in lower case as PostgreSQL lists it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.0.0
    }

    /// The SQLSTATE a `RAISE` of the name reports, the first PostgreSQL lists
    /// for it.
    #[must_use]
    pub fn sqlstate(self) -> SqlState {
        self.0.1
    }

    /// Every SQLSTATE PostgreSQL lists for the name, in its order, each of
    /// which a handler naming it catches.
    ///
    /// Four names stand for two codes each, `null_value_not_allowed`,
    /// `modifying_sql_data_not_permitted`,
    /// `prohibited_sql_statement_attempted` and
    /// `reading_sql_data_not_permitted`.
    pub fn sqlstates(self) -> impl Iterator<Item = SqlState> {
        CONDITIONS
            .iter()
            .filter(move |(name, _)| *name == self.as_str())
            .map(|&(_, code)| code)
    }
}

impl FromStr for ConditionName {
    type Err = UnknownCondition;

    /// Looks `name` up as written, so only its lower-case spelling is known.
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        CONDITIONS
            .iter()
            .find(|(known, _)| *known == name)
            .map(Self)
            .ok_or_else(|| UnknownCondition { name: name.into() })
    }
}

impl fmt::Debug for ConditionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ConditionName")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for ConditionName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A name PL/pgSQL knows no condition by, which PostgreSQL refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unrecognized exception condition \"{name}\"")]
pub struct UnknownCondition {
    /// The name, as looked up.
    pub name: String,
}

/// The SQLSTATE `RAISE ... USING ERRCODE = text` reports, reading `text` as
/// PL/pgSQL does when the `RAISE` runs.
///
/// Five digits or upper-case ASCII letters are a code, and any other text is
/// a condition name looked up as written. `00000` is no SQLSTATE, so the
/// `RAISE` reports the code of its level, `P0001` for `EXCEPTION`.
///
/// # Errors
///
/// Returns [`UnknownCondition`] when `text` is neither a code nor a condition
/// name, which PostgreSQL refuses when the `RAISE` runs.
pub fn errcode(text: &str) -> Result<Option<SqlState>, UnknownCondition> {
    match text.parse::<SqlState>() {
        Ok(code) => Ok(reported(code)),
        Err(_) => text
            .parse()
            .map(|name: ConditionName| Some(name.sqlstate())),
    }
}

/// The SQLSTATE a `RAISE` of `code` reports, none for `00000`, which
/// PL/pgSQL reads as no code.
pub(crate) fn reported(code: SqlState) -> Option<SqlState> {
    (code != pg::SUCCESSFUL_COMPLETION).then_some(code)
}

/// Every error condition name with its SQLSTATE, in the order of PostgreSQL
/// 18's `errcodes.txt`, from which `plerrcodes.h` keeps each error that has a
/// PL/pgSQL name.
#[rustfmt::skip]
static CONDITIONS: [(&str, SqlState); 251] = [
    ("sql_statement_not_yet_complete", pg::SQL_STATEMENT_NOT_YET_COMPLETE),
    ("connection_exception", pg::CONNECTION_EXCEPTION),
    ("connection_does_not_exist", pg::CONNECTION_DOES_NOT_EXIST),
    ("connection_failure", pg::CONNECTION_FAILURE),
    ("sqlclient_unable_to_establish_sqlconnection", pg::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION),
    ("sqlserver_rejected_establishment_of_sqlconnection", pg::SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION),
    ("transaction_resolution_unknown", pg::TRANSACTION_RESOLUTION_UNKNOWN),
    ("protocol_violation", pg::PROTOCOL_VIOLATION),
    ("triggered_action_exception", pg::TRIGGERED_ACTION_EXCEPTION),
    ("feature_not_supported", pg::FEATURE_NOT_SUPPORTED),
    ("invalid_transaction_initiation", pg::INVALID_TRANSACTION_INITIATION),
    ("locator_exception", pg::LOCATOR_EXCEPTION),
    ("invalid_locator_specification", pg::L_E_INVALID_SPECIFICATION),
    ("invalid_grantor", pg::INVALID_GRANTOR),
    ("invalid_grant_operation", pg::INVALID_GRANT_OPERATION),
    ("invalid_role_specification", pg::INVALID_ROLE_SPECIFICATION),
    ("diagnostics_exception", pg::DIAGNOSTICS_EXCEPTION),
    ("stacked_diagnostics_accessed_without_active_handler", pg::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER),
    ("invalid_argument_for_xquery", pg::INVALID_ARGUMENT_FOR_XQUERY),
    ("case_not_found", pg::CASE_NOT_FOUND),
    ("cardinality_violation", pg::CARDINALITY_VIOLATION),
    ("data_exception", pg::DATA_EXCEPTION),
    ("array_subscript_error", pg::ARRAY_SUBSCRIPT_ERROR),
    ("character_not_in_repertoire", pg::CHARACTER_NOT_IN_REPERTOIRE),
    ("datetime_field_overflow", pg::DATETIME_FIELD_OVERFLOW),
    ("division_by_zero", pg::DIVISION_BY_ZERO),
    ("error_in_assignment", pg::ERROR_IN_ASSIGNMENT),
    ("escape_character_conflict", pg::ESCAPE_CHARACTER_CONFLICT),
    ("indicator_overflow", pg::INDICATOR_OVERFLOW),
    ("interval_field_overflow", pg::INTERVAL_FIELD_OVERFLOW),
    ("invalid_argument_for_logarithm", pg::INVALID_ARGUMENT_FOR_LOG),
    ("invalid_argument_for_ntile_function", pg::INVALID_ARGUMENT_FOR_NTILE),
    ("invalid_argument_for_nth_value_function", pg::INVALID_ARGUMENT_FOR_NTH_VALUE),
    ("invalid_argument_for_power_function", pg::INVALID_ARGUMENT_FOR_POWER_FUNCTION),
    ("invalid_argument_for_width_bucket_function", pg::INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION),
    ("invalid_character_value_for_cast", pg::INVALID_CHARACTER_VALUE_FOR_CAST),
    ("invalid_datetime_format", pg::INVALID_DATETIME_FORMAT),
    ("invalid_escape_character", pg::INVALID_ESCAPE_CHARACTER),
    ("invalid_escape_octet", pg::INVALID_ESCAPE_OCTET),
    ("invalid_escape_sequence", pg::INVALID_ESCAPE_SEQUENCE),
    ("nonstandard_use_of_escape_character", pg::NONSTANDARD_USE_OF_ESCAPE_CHARACTER),
    ("invalid_indicator_parameter_value", pg::INVALID_INDICATOR_PARAMETER_VALUE),
    ("invalid_parameter_value", pg::INVALID_PARAMETER_VALUE),
    ("invalid_preceding_or_following_size", pg::INVALID_PRECEDING_OR_FOLLOWING_SIZE),
    ("invalid_regular_expression", pg::INVALID_REGULAR_EXPRESSION),
    ("invalid_row_count_in_limit_clause", pg::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE),
    ("invalid_row_count_in_result_offset_clause", pg::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE),
    ("invalid_tablesample_argument", pg::INVALID_TABLESAMPLE_ARGUMENT),
    ("invalid_tablesample_repeat", pg::INVALID_TABLESAMPLE_REPEAT),
    ("invalid_time_zone_displacement_value", pg::INVALID_TIME_ZONE_DISPLACEMENT_VALUE),
    ("invalid_use_of_escape_character", pg::INVALID_USE_OF_ESCAPE_CHARACTER),
    ("most_specific_type_mismatch", pg::MOST_SPECIFIC_TYPE_MISMATCH),
    ("null_value_not_allowed", pg::NULL_VALUE_NOT_ALLOWED),
    ("null_value_no_indicator_parameter", pg::NULL_VALUE_NO_INDICATOR_PARAMETER),
    ("numeric_value_out_of_range", pg::NUMERIC_VALUE_OUT_OF_RANGE),
    ("sequence_generator_limit_exceeded", pg::SEQUENCE_GENERATOR_LIMIT_EXCEEDED),
    ("string_data_length_mismatch", pg::STRING_DATA_LENGTH_MISMATCH),
    ("string_data_right_truncation", pg::STRING_DATA_RIGHT_TRUNCATION),
    ("substring_error", pg::SUBSTRING_ERROR),
    ("trim_error", pg::TRIM_ERROR),
    ("unterminated_c_string", pg::UNTERMINATED_C_STRING),
    ("zero_length_character_string", pg::ZERO_LENGTH_CHARACTER_STRING),
    ("floating_point_exception", pg::FLOATING_POINT_EXCEPTION),
    ("invalid_text_representation", pg::INVALID_TEXT_REPRESENTATION),
    ("invalid_binary_representation", pg::INVALID_BINARY_REPRESENTATION),
    ("bad_copy_file_format", pg::BAD_COPY_FILE_FORMAT),
    ("untranslatable_character", pg::UNTRANSLATABLE_CHARACTER),
    ("not_an_xml_document", pg::NOT_AN_XML_DOCUMENT),
    ("invalid_xml_document", pg::INVALID_XML_DOCUMENT),
    ("invalid_xml_content", pg::INVALID_XML_CONTENT),
    ("invalid_xml_comment", pg::INVALID_XML_COMMENT),
    ("invalid_xml_processing_instruction", pg::INVALID_XML_PROCESSING_INSTRUCTION),
    ("duplicate_json_object_key_value", pg::DUPLICATE_JSON_OBJECT_KEY_VALUE),
    ("invalid_argument_for_sql_json_datetime_function", pg::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION),
    ("invalid_json_text", pg::INVALID_JSON_TEXT),
    ("invalid_sql_json_subscript", pg::INVALID_SQL_JSON_SUBSCRIPT),
    ("more_than_one_sql_json_item", pg::MORE_THAN_ONE_SQL_JSON_ITEM),
    ("no_sql_json_item", pg::NO_SQL_JSON_ITEM),
    ("non_numeric_sql_json_item", pg::NON_NUMERIC_SQL_JSON_ITEM),
    ("non_unique_keys_in_a_json_object", pg::NON_UNIQUE_KEYS_IN_A_JSON_OBJECT),
    ("singleton_sql_json_item_required", pg::SINGLETON_SQL_JSON_ITEM_REQUIRED),
    ("sql_json_array_not_found", pg::SQL_JSON_ARRAY_NOT_FOUND),
    ("sql_json_member_not_found", pg::SQL_JSON_MEMBER_NOT_FOUND),
    ("sql_json_number_not_found", pg::SQL_JSON_NUMBER_NOT_FOUND),
    ("sql_json_object_not_found", pg::SQL_JSON_OBJECT_NOT_FOUND),
    ("too_many_json_array_elements", pg::TOO_MANY_JSON_ARRAY_ELEMENTS),
    ("too_many_json_object_members", pg::TOO_MANY_JSON_OBJECT_MEMBERS),
    ("sql_json_scalar_required", pg::SQL_JSON_SCALAR_REQUIRED),
    ("sql_json_item_cannot_be_cast_to_target_type", pg::SQL_JSON_ITEM_CANNOT_BE_CAST_TO_TARGET_TYPE),
    ("integrity_constraint_violation", pg::INTEGRITY_CONSTRAINT_VIOLATION),
    ("restrict_violation", pg::RESTRICT_VIOLATION),
    ("not_null_violation", pg::NOT_NULL_VIOLATION),
    ("foreign_key_violation", pg::FOREIGN_KEY_VIOLATION),
    ("unique_violation", pg::UNIQUE_VIOLATION),
    ("check_violation", pg::CHECK_VIOLATION),
    ("exclusion_violation", pg::EXCLUSION_VIOLATION),
    ("invalid_cursor_state", pg::INVALID_CURSOR_STATE),
    ("invalid_transaction_state", pg::INVALID_TRANSACTION_STATE),
    ("active_sql_transaction", pg::ACTIVE_SQL_TRANSACTION),
    ("branch_transaction_already_active", pg::BRANCH_TRANSACTION_ALREADY_ACTIVE),
    ("held_cursor_requires_same_isolation_level", pg::HELD_CURSOR_REQUIRES_SAME_ISOLATION_LEVEL),
    ("inappropriate_access_mode_for_branch_transaction", pg::INAPPROPRIATE_ACCESS_MODE_FOR_BRANCH_TRANSACTION),
    ("inappropriate_isolation_level_for_branch_transaction", pg::INAPPROPRIATE_ISOLATION_LEVEL_FOR_BRANCH_TRANSACTION),
    ("no_active_sql_transaction_for_branch_transaction", pg::NO_ACTIVE_SQL_TRANSACTION_FOR_BRANCH_TRANSACTION),
    ("read_only_sql_transaction", pg::READ_ONLY_SQL_TRANSACTION),
    ("schema_and_data_statement_mixing_not_supported", pg::SCHEMA_AND_DATA_STATEMENT_MIXING_NOT_SUPPORTED),
    ("no_active_sql_transaction", pg::NO_ACTIVE_SQL_TRANSACTION),
    ("in_failed_sql_transaction", pg::IN_FAILED_SQL_TRANSACTION),
    ("idle_in_transaction_session_timeout", pg::IDLE_IN_TRANSACTION_SESSION_TIMEOUT),
    ("transaction_timeout", pg::TRANSACTION_TIMEOUT),
    ("invalid_sql_statement_name", pg::INVALID_SQL_STATEMENT_NAME),
    ("triggered_data_change_violation", pg::TRIGGERED_DATA_CHANGE_VIOLATION),
    ("invalid_authorization_specification", pg::INVALID_AUTHORIZATION_SPECIFICATION),
    ("invalid_password", pg::INVALID_PASSWORD),
    ("dependent_privilege_descriptors_still_exist", pg::DEPENDENT_PRIVILEGE_DESCRIPTORS_STILL_EXIST),
    ("dependent_objects_still_exist", pg::DEPENDENT_OBJECTS_STILL_EXIST),
    ("invalid_transaction_termination", pg::INVALID_TRANSACTION_TERMINATION),
    ("sql_routine_exception", pg::SQL_ROUTINE_EXCEPTION),
    ("function_executed_no_return_statement", pg::S_R_E_FUNCTION_EXECUTED_NO_RETURN_STATEMENT),
    ("modifying_sql_data_not_permitted", pg::S_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED),
    ("prohibited_sql_statement_attempted", pg::S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED),
    ("reading_sql_data_not_permitted", pg::S_R_E_READING_SQL_DATA_NOT_PERMITTED),
    ("invalid_cursor_name", pg::INVALID_CURSOR_NAME),
    ("external_routine_exception", pg::EXTERNAL_ROUTINE_EXCEPTION),
    ("containing_sql_not_permitted", pg::E_R_E_CONTAINING_SQL_NOT_PERMITTED),
    ("modifying_sql_data_not_permitted", pg::E_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED),
    ("prohibited_sql_statement_attempted", pg::E_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED),
    ("reading_sql_data_not_permitted", pg::E_R_E_READING_SQL_DATA_NOT_PERMITTED),
    ("external_routine_invocation_exception", pg::EXTERNAL_ROUTINE_INVOCATION_EXCEPTION),
    ("invalid_sqlstate_returned", pg::E_R_I_E_INVALID_SQLSTATE_RETURNED),
    ("null_value_not_allowed", pg::E_R_I_E_NULL_VALUE_NOT_ALLOWED),
    ("trigger_protocol_violated", pg::E_R_I_E_TRIGGER_PROTOCOL_VIOLATED),
    ("srf_protocol_violated", pg::E_R_I_E_SRF_PROTOCOL_VIOLATED),
    ("event_trigger_protocol_violated", pg::E_R_I_E_EVENT_TRIGGER_PROTOCOL_VIOLATED),
    ("savepoint_exception", pg::SAVEPOINT_EXCEPTION),
    ("invalid_savepoint_specification", pg::S_E_INVALID_SPECIFICATION),
    ("invalid_catalog_name", pg::INVALID_CATALOG_NAME),
    ("invalid_schema_name", pg::INVALID_SCHEMA_NAME),
    ("transaction_rollback", pg::TRANSACTION_ROLLBACK),
    ("transaction_integrity_constraint_violation", pg::T_R_INTEGRITY_CONSTRAINT_VIOLATION),
    ("serialization_failure", pg::T_R_SERIALIZATION_FAILURE),
    ("statement_completion_unknown", pg::T_R_STATEMENT_COMPLETION_UNKNOWN),
    ("deadlock_detected", pg::T_R_DEADLOCK_DETECTED),
    ("syntax_error_or_access_rule_violation", pg::SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION),
    ("syntax_error", pg::SYNTAX_ERROR),
    ("insufficient_privilege", pg::INSUFFICIENT_PRIVILEGE),
    ("cannot_coerce", pg::CANNOT_COERCE),
    ("grouping_error", pg::GROUPING_ERROR),
    ("windowing_error", pg::WINDOWING_ERROR),
    ("invalid_recursion", pg::INVALID_RECURSION),
    ("invalid_foreign_key", pg::INVALID_FOREIGN_KEY),
    ("invalid_name", pg::INVALID_NAME),
    ("name_too_long", pg::NAME_TOO_LONG),
    ("reserved_name", pg::RESERVED_NAME),
    ("datatype_mismatch", pg::DATATYPE_MISMATCH),
    ("indeterminate_datatype", pg::INDETERMINATE_DATATYPE),
    ("collation_mismatch", pg::COLLATION_MISMATCH),
    ("indeterminate_collation", pg::INDETERMINATE_COLLATION),
    ("wrong_object_type", pg::WRONG_OBJECT_TYPE),
    ("generated_always", pg::GENERATED_ALWAYS),
    ("undefined_column", pg::UNDEFINED_COLUMN),
    ("undefined_function", pg::UNDEFINED_FUNCTION),
    ("undefined_table", pg::UNDEFINED_TABLE),
    ("undefined_parameter", pg::UNDEFINED_PARAMETER),
    ("undefined_object", pg::UNDEFINED_OBJECT),
    ("duplicate_column", pg::DUPLICATE_COLUMN),
    ("duplicate_cursor", pg::DUPLICATE_CURSOR),
    ("duplicate_database", pg::DUPLICATE_DATABASE),
    ("duplicate_function", pg::DUPLICATE_FUNCTION),
    ("duplicate_prepared_statement", pg::DUPLICATE_PSTATEMENT),
    ("duplicate_schema", pg::DUPLICATE_SCHEMA),
    ("duplicate_table", pg::DUPLICATE_TABLE),
    ("duplicate_alias", pg::DUPLICATE_ALIAS),
    ("duplicate_object", pg::DUPLICATE_OBJECT),
    ("ambiguous_column", pg::AMBIGUOUS_COLUMN),
    ("ambiguous_function", pg::AMBIGUOUS_FUNCTION),
    ("ambiguous_parameter", pg::AMBIGUOUS_PARAMETER),
    ("ambiguous_alias", pg::AMBIGUOUS_ALIAS),
    ("invalid_column_reference", pg::INVALID_COLUMN_REFERENCE),
    ("invalid_column_definition", pg::INVALID_COLUMN_DEFINITION),
    ("invalid_cursor_definition", pg::INVALID_CURSOR_DEFINITION),
    ("invalid_database_definition", pg::INVALID_DATABASE_DEFINITION),
    ("invalid_function_definition", pg::INVALID_FUNCTION_DEFINITION),
    ("invalid_prepared_statement_definition", pg::INVALID_PSTATEMENT_DEFINITION),
    ("invalid_schema_definition", pg::INVALID_SCHEMA_DEFINITION),
    ("invalid_table_definition", pg::INVALID_TABLE_DEFINITION),
    ("invalid_object_definition", pg::INVALID_OBJECT_DEFINITION),
    ("with_check_option_violation", pg::WITH_CHECK_OPTION_VIOLATION),
    ("insufficient_resources", pg::INSUFFICIENT_RESOURCES),
    ("disk_full", pg::DISK_FULL),
    ("out_of_memory", pg::OUT_OF_MEMORY),
    ("too_many_connections", pg::TOO_MANY_CONNECTIONS),
    ("configuration_limit_exceeded", pg::CONFIGURATION_LIMIT_EXCEEDED),
    ("program_limit_exceeded", pg::PROGRAM_LIMIT_EXCEEDED),
    ("statement_too_complex", pg::STATEMENT_TOO_COMPLEX),
    ("too_many_columns", pg::TOO_MANY_COLUMNS),
    ("too_many_arguments", pg::TOO_MANY_ARGUMENTS),
    ("object_not_in_prerequisite_state", pg::OBJECT_NOT_IN_PREREQUISITE_STATE),
    ("object_in_use", pg::OBJECT_IN_USE),
    ("cant_change_runtime_param", pg::CANT_CHANGE_RUNTIME_PARAM),
    ("lock_not_available", pg::LOCK_NOT_AVAILABLE),
    ("unsafe_new_enum_value_usage", pg::UNSAFE_NEW_ENUM_VALUE_USAGE),
    ("operator_intervention", pg::OPERATOR_INTERVENTION),
    ("query_canceled", pg::QUERY_CANCELED),
    ("admin_shutdown", pg::ADMIN_SHUTDOWN),
    ("crash_shutdown", pg::CRASH_SHUTDOWN),
    ("cannot_connect_now", pg::CANNOT_CONNECT_NOW),
    ("database_dropped", pg::DATABASE_DROPPED),
    ("idle_session_timeout", pg::IDLE_SESSION_TIMEOUT),
    ("system_error", pg::SYSTEM_ERROR),
    ("io_error", pg::IO_ERROR),
    ("undefined_file", pg::UNDEFINED_FILE),
    ("duplicate_file", pg::DUPLICATE_FILE),
    ("file_name_too_long", pg::FILE_NAME_TOO_LONG),
    ("config_file_error", pg::CONFIG_FILE_ERROR),
    ("lock_file_exists", pg::LOCK_FILE_EXISTS),
    ("fdw_error", pg::FDW_ERROR),
    ("fdw_column_name_not_found", pg::FDW_COLUMN_NAME_NOT_FOUND),
    ("fdw_dynamic_parameter_value_needed", pg::FDW_DYNAMIC_PARAMETER_VALUE_NEEDED),
    ("fdw_function_sequence_error", pg::FDW_FUNCTION_SEQUENCE_ERROR),
    ("fdw_inconsistent_descriptor_information", pg::FDW_INCONSISTENT_DESCRIPTOR_INFORMATION),
    ("fdw_invalid_attribute_value", pg::FDW_INVALID_ATTRIBUTE_VALUE),
    ("fdw_invalid_column_name", pg::FDW_INVALID_COLUMN_NAME),
    ("fdw_invalid_column_number", pg::FDW_INVALID_COLUMN_NUMBER),
    ("fdw_invalid_data_type", pg::FDW_INVALID_DATA_TYPE),
    ("fdw_invalid_data_type_descriptors", pg::FDW_INVALID_DATA_TYPE_DESCRIPTORS),
    ("fdw_invalid_descriptor_field_identifier", pg::FDW_INVALID_DESCRIPTOR_FIELD_IDENTIFIER),
    ("fdw_invalid_handle", pg::FDW_INVALID_HANDLE),
    ("fdw_invalid_option_index", pg::FDW_INVALID_OPTION_INDEX),
    ("fdw_invalid_option_name", pg::FDW_INVALID_OPTION_NAME),
    ("fdw_invalid_string_length_or_buffer_length", pg::FDW_INVALID_STRING_LENGTH_OR_BUFFER_LENGTH),
    ("fdw_invalid_string_format", pg::FDW_INVALID_STRING_FORMAT),
    ("fdw_invalid_use_of_null_pointer", pg::FDW_INVALID_USE_OF_NULL_POINTER),
    ("fdw_too_many_handles", pg::FDW_TOO_MANY_HANDLES),
    ("fdw_out_of_memory", pg::FDW_OUT_OF_MEMORY),
    ("fdw_no_schemas", pg::FDW_NO_SCHEMAS),
    ("fdw_option_name_not_found", pg::FDW_OPTION_NAME_NOT_FOUND),
    ("fdw_reply_handle", pg::FDW_REPLY_HANDLE),
    ("fdw_schema_not_found", pg::FDW_SCHEMA_NOT_FOUND),
    ("fdw_table_not_found", pg::FDW_TABLE_NOT_FOUND),
    ("fdw_unable_to_create_execution", pg::FDW_UNABLE_TO_CREATE_EXECUTION),
    ("fdw_unable_to_create_reply", pg::FDW_UNABLE_TO_CREATE_REPLY),
    ("fdw_unable_to_establish_connection", pg::FDW_UNABLE_TO_ESTABLISH_CONNECTION),
    ("plpgsql_error", pg::PLPGSQL_ERROR),
    ("raise_exception", pg::RAISE_EXCEPTION),
    ("no_data_found", pg::NO_DATA_FOUND),
    ("too_many_rows", pg::TOO_MANY_ROWS),
    ("assert_failure", pg::ASSERT_FAILURE),
    ("internal_error", pg::INTERNAL_ERROR),
    ("data_corrupted", pg::DATA_CORRUPTED),
    ("index_corrupted", pg::INDEX_CORRUPTED),
];
