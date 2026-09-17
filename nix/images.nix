{ pkgs, application }:
let
  identity = pkgs.runCommand "grading-identities" { } ''
    mkdir -p $out/etc $out/tmp $out/var/lib/grading
    echo 'grading:x:10001:10001:Grading:/var/lib/grading:/sbin/nologin' > $out/etc/passwd
    echo 'postgres:x:10002:10002:PostgreSQL:/var/lib/postgresql:/sbin/nologin' >> $out/etc/passwd
    echo 'executor:x:10004:10004:Executor:/var/lib/executor:/sbin/nologin' >> $out/etc/passwd
    printf 'grading:x:10001:\npostgres:x:10002:\nexecutor:x:10004:\n' > $out/etc/group
    chmod 1777 $out/tmp
  '';
  databaseStart = pkgs.writeShellApplication {
    name = "grading-postgres";
    runtimeInputs = [
      pkgs.postgresql_18
      pkgs.coreutils
    ];
    text = builtins.readFile ./postgres-entrypoint.sh;
  };
in
{
  runner-image = pkgs.dockerTools.buildLayeredImage {
    name = "grading-runner";
    tag = "prototype";
    contents = [
      pkgs.bash
      pkgs.coreutils
      pkgs.python3
      pkgs.gcc
    ];
    config = {
      User = "10003:10003";
      Env = [
        "PATH=/bin"
        "PYTHONDONTWRITEBYTECODE=1"
      ];
    };
  };
  web-image = pkgs.dockerTools.buildLayeredImage {
    name = "grading-web";
    tag = "prototype";
    contents = [
      application
      identity
      pkgs.cacert
    ];
    config = {
      User = "10001:10001";
      Entrypoint = [ "${application}/bin/grading-web" ];
      Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
    };
  };
  cli-image = pkgs.dockerTools.buildLayeredImage {
    name = "grading-cli";
    tag = "prototype";
    contents = [
      application
      identity
      pkgs.cacert
      pkgs.git
      pkgs.postgresql_18
    ];
    config = {
      User = "10001:10001";
      Entrypoint = [ "${application}/bin/gradingctl" ];
      Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
    };
  };
  executor-image = pkgs.dockerTools.buildLayeredImage {
    name = "grading-executor";
    tag = "prototype";
    contents = [
      application
      identity
      pkgs.cacert
    ];
    config = {
      User = "10004:10004";
      Entrypoint = [ "${application}/bin/grading-executor" ];
      Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
    };
  };
  postgres-image = pkgs.dockerTools.buildLayeredImage {
    name = "grading-postgres";
    tag = "prototype";
    contents = [
      pkgs.bash
      pkgs.postgresql_18
      databaseStart
      identity
    ];
    config = {
      User = "10002:10002";
      Entrypoint = [ "${databaseStart}/bin/grading-postgres" ];
      Env = [
        "PGDATA=/var/lib/postgresql/data"
        "PGHOST=/run/postgresql"
        "PGUSER=postgres"
      ];
    };
  };
}
