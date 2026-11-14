; Symbols of a Python source file. Capture names: see queries/rust/symbols.scm.

(function_definition
  name: (identifier) @name
  parameters: (parameters) @sig
  return_type: (_)? @sig
  body: (block) @body) @def.function

(class_definition
  name: (identifier) @name
  superclasses: (_)? @sig
  body: (block) @body) @def.class

; With decorators the definition is the decorated node, so that the
; decorators travel with the symbol.
(decorated_definition
  definition: (function_definition
    name: (identifier) @name
    parameters: (parameters) @sig
    return_type: (_)? @sig
    body: (block) @body)) @def.function

(decorated_definition
  definition: (class_definition
    name: (identifier) @name
    superclasses: (_)? @sig
    body: (block) @body)) @def.class

; Module-level assignments.
(module (expression_statement (assignment left: (identifier) @name) @def.var))
