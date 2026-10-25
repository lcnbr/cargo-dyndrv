{
  lib,
  runCommand,
  makeWrapper,
  python3,
  cargo,
  git,
  jujutsu,
}:
let
  python = python3.withPackages (ps: [ ps.tomli-w ]);
  pythonFiles = path: lib.fileset.fileFilter (file: lib.hasSuffix ".py" file.name) path;
  source = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      (pythonFiles ../worker)
      ../tests/worker/test_sources.py
      ../tests/worker/test_worker.py
    ];
  };
  testsSource = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      (pythonFiles ../worker)
      (pythonFiles ../tests/worker)
    ];
  };
in
runCommand "cargo-dyndrv-worker"
  {
    nativeBuildInputs = [
      makeWrapper
      python
      cargo
      git
      jujutsu
    ];
    meta.mainProgram = "cargo-dyndrv-worker";
    meta.platforms = lib.platforms.linux;
    passthru.testsSource = testsSource;
  }
  ''
    export PYTHONDONTWRITEBYTECODE=1
    python3 ${source}/tests/worker/test_sources.py
    python3 ${source}/tests/worker/test_worker.py
    mkdir -p "$out/share/cargo-dyndrv" "$out/bin"
    cp ${source}/worker/*.py "$out/share/cargo-dyndrv/"
    makeWrapper ${python}/bin/python3 "$out/bin/cargo-dyndrv-worker" \
      --add-flags "$out/share/cargo-dyndrv/worker.py"
  ''
