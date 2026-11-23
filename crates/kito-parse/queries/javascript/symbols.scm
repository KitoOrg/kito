; Symbols of a JavaScript source file. Capture names: see queries/rust/symbols.scm.

(function_declaration
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  body: (statement_block) @body) @def.function

(generator_function_declaration
  name: (identifier) @name
  parameters: (formal_parameters) @sig
  body: (statement_block) @body) @def.function

(class_declaration name: (identifier) @name body: (class_body) @body) @def.class

; The setter of a property has the name of its getter: `set` is part of
; its own, `set total`. The getter keeps the plain name.
(method_definition
  "set" @role
  name: (_) @name
  parameters: (formal_parameters) @sig
  body: (statement_block) @body) @def.method

(method_definition
  name: (_) @name
  parameters: (formal_parameters) @sig
  body: (statement_block) @body) @def.method

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
