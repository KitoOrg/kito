; Symbols of a Go source file. Capture names: see queries/rust/symbols.scm.

(function_declaration
  name: (identifier) @name
  parameters: (parameter_list) @sig
  result: (_)? @sig
  body: (block)? @body) @def.function

; Methods are named after their receiver type: `Server.Start`.
(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (type_identifier) @scope
        (pointer_type (type_identifier) @scope)
        (generic_type type: (type_identifier) @scope)
        (pointer_type (generic_type type: (type_identifier) @scope))
      ]))
  name: (field_identifier) @name
  parameters: (parameter_list) @sig
  result: (_)? @sig
  body: (block)? @body) @def.method

; A declaration with a single spec is the symbol as a whole (keyword
; included); inside a group each spec is a symbol of its own.
(type_declaration . (type_spec name: (type_identifier) @name type: (struct_type) @body) .) @def.struct
(type_declaration . (type_spec name: (type_identifier) @name type: (interface_type) @body) .) @def.interface
(type_declaration . (type_spec name: (type_identifier) @name type: (_) @body) .) @def.type_alias
(type_declaration . (type_alias name: (type_identifier) @name type: (_) @body) .) @def.type_alias
(type_spec name: (type_identifier) @name type: (struct_type) @body) @def.struct
(type_spec name: (type_identifier) @name type: (interface_type) @body) @def.interface
(type_spec name: (type_identifier) @name type: (_) @body) @def.type_alias
(type_alias name: (type_identifier) @name type: (_) @body) @def.type_alias

(source_file (const_declaration (const_spec name: (identifier) @name) @def.const))
(source_file (var_declaration (var_spec name: (identifier) @name) @def.var))
