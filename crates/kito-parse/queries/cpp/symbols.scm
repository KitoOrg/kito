; Symbols of a C++ source file. Capture names: see queries/rust/symbols.scm.

; Free functions, methods defined in a class body (field_identifier) and
; out-of-class definitions (qualified_identifier, e.g. Foo::bar).
(function_definition
  declarator: [
    (function_declarator
      declarator: [(identifier) (field_identifier) (qualified_identifier) (destructor_name) (operator_name)] @name
      parameters: (parameter_list) @sig)
    (pointer_declarator
      declarator: (function_declarator
        declarator: [(identifier) (field_identifier) (qualified_identifier) (operator_name)] @name
        parameters: (parameter_list) @sig))
    (reference_declarator
      (function_declarator
        declarator: [(identifier) (field_identifier) (qualified_identifier) (operator_name)] @name
        parameters: (parameter_list) @sig))
  ]
  body: (_) @body) @def.function

(class_specifier name: (_) @name body: (field_declaration_list) @body) @def.class
(struct_specifier name: (_) @name body: (field_declaration_list) @body) @def.struct
(union_specifier name: (_) @name body: (field_declaration_list) @body) @def.struct
(enum_specifier name: (_) @name body: (enumerator_list) @body) @def.enum
(namespace_definition name: (_) @name body: (declaration_list) @body) @def.module

(type_definition declarator: (type_identifier) @name) @def.type_alias
(alias_declaration name: (type_identifier) @name) @def.type_alias

(preproc_function_def name: (identifier) @name) @def.function
(preproc_def name: (identifier) @name) @def.const
