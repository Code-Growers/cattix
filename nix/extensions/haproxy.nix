{ lib, ... }:

with lib;

let
  haproxyType = types.submodule {
    options = {
      sockets = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "HAProxy runtime API sockets used to drain and enable the server.";
      };
      backend = mkOption {
        type = types.str;
        default = "";
        description = "HAProxy backend containing the managed server.";
      };
      server = mkOption {
        type = types.str;
        default = "";
        description = "HAProxy server name to drain or enable.";
      };
    };
  };
in
{
  options.cattix.extensions.haproxy = mkOption {
    type = types.nullOr haproxyType;
    default = null;
    description = "HAProxy configuration for load-balancer draining during rollouts.";
  };
}
