; Symbols of a TypeScript or TSX source file. Capture names: see queries/rust/symbols.scm.

(function_declaration
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig
  body: (statement_block) @body) @def.function

(generator_function_declaration
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig
  body: (statement_block) @body) @def.function

; Overload and ambient signatures.
(function_signature
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig) @def.function

(class_declaration name: (type_identifier) @name body: (class_body) @body) @def.class
(abstract_class_declaration name: (type_identifier) @name body: (class_body) @body) @def.class
(interface_declaration name: (type_identifier) @name body: (_) @body) @def.interface
(type_alias_declaration name: (type_identifier) @name value: (_) @body) @def.type_alias
(enum_declaration name: (identifier) @name body: (enum_body) @body) @def.enum
(internal_module name: (_) @name body: (statement_block) @body) @def.module
(module name: (_) @name body: (statement_block) @body) @def.module

; The setter of a property has the name of its getter: `set` is part of
; its own, `set total`. The getter keeps the plain name.
(method_definition
  "set" @role
  name: (_) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig
  body: (statement_block) @body) @def.method

(abstract_method_signature
  "set" @role
  name: (_) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig) @def.method

(method_definition
  name: (_) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig
  body: (statement_block) @body) @def.method

(abstract_method_signature
  name: (_) @name
  parameters: (formal_parameters) @sig
  return_type: (_)? @sig) @def.method

; const f = () => { … } and const f = function () { … }
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)] @body)) @def.function

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)] @body)) @def.function

; Other top-level constants.
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name value: (_) @body)) @def.const)

(program
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator name: (identifier) @name value: (_) @body)) @def.const))
