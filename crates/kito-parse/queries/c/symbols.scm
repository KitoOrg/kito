; Symbols of a C source file. Capture names: see queries/rust/symbols.scm.

; The declarator of a function is wrapped once per `*` of its return type.
(function_definition
  declarator: [
    (function_declarator
      declarator: (identifier) @name
      parameters: (parameter_list) @sig)
    (pointer_declarator
      declarator: (function_declarator
        declarator: (identifier) @name
        parameters: (parameter_list) @sig))
    (pointer_declarator
      declarator: (pointer_declarator
        declarator: (function_declarator
          declarator: (identifier) @name
          parameters: (parameter_list) @sig)))
  ]
  body: (compound_statement) @body) @def.function

(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @def.struct
(union_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @def.struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @def.enum
(type_definition declarator: (type_identifier) @name) @def.type_alias

(preproc_function_def name: (identifier) @name) @def.function
(preproc_def name: (identifier) @name) @def.const
