{
  description = "Example public runner and isolated private checker";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = nixpkgs.lib.genAttrs systems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          student = pkgs.writeShellScriptBin "student" ''
            export PATH=${pkgs.coreutils}/bin
            exec ${pkgs.bash}/bin/bash /workspace/src/solution.sh
          '';
          grade = pkgs.writeShellScriptBin "grade" ''
            exec ${pkgs.python3}/bin/python3 ${./grade.py}
          '';
        in
        {
          studentImage = pkgs.dockerTools.buildLayeredImage {
            name = "exercise-student";
            tag = "build";
            contents = [
              student
              pkgs.bash
              pkgs.coreutils
            ];
            config.User = "10003:10003";
          };
          graderImage = pkgs.dockerTools.buildLayeredImage {
            name = "exercise-grader";
            tag = "build";
            contents = [ grade ];
            config.User = "10003:10003";
          };
        }
      );
    };
}
