{
  lib,
  runCommand,
  jq,
  writers,
}:
let
  singlePackage =
    name: args:
    let
      baseJSON = writers.writeJSON "base.json" args;
    in
    runCommand "part.json"
      {
        __structuredAttrs = true;
        exportReferencesGraph.graph = baseJSON;
        preferLocalBuild = true;
        nativeBuildInputs = [ jq ];
      }
      ''
        jq -s '{"${name}": {inputs: .[0].graph[] | select(.path == "${baseJSON}") | .references} * .[1]}' "$NIX_ATTRS_JSON_FILE" "${baseJSON}" > $out
      '';
in
extern:
let
  parts = lib.mapAttrsToList singlePackage extern;
in
runCommand "extern.json"
  {
    preferLocalBuild = true;
    nativeBuildInputs = [ jq ];
  }
  ''
    jq -n 'reduce inputs as $item ({}; . * $item)' ${lib.escapeShellArgs parts} > $out
  ''
