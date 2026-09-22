{ lib }:

{
  hosts ? null,
  nixosConfigurations ? null,
}:

let
  configurations =
    if hosts != null && nixosConfigurations != null then
      throw "cattix.lib.mkFleet accepts either hosts or nixosConfigurations, not both"
    else if hosts != null then
      hosts
    else if nixosConfigurations != null then
      nixosConfigurations
    else
      throw "cattix.lib.mkFleet requires a hosts attribute set";

  hostNames = builtins.attrNames configurations;

  mkHost = name:
    let
      config = configurations.${name}.config;
    in
    {
      inherit name;
      group = config.cattix.group;
      order = config.cattix.order;
      target = config.cattix.target;
      metadata = config.cattix.metadata;
      extensions = config.cattix.extensions or {};
      healthChecks = config.cattix.healthChecks;
      expectedBuild = config.system.build.toplevel;
      configurationRevision = config.system.configurationRevision;
    };

  fleetHosts = map mkHost hostNames;
  groupNames = lib.unique (builtins.filter (group: group != null) (map (host: host.group) fleetHosts));
in
{
  version = 1;
  hosts = fleetHosts;
  groups = builtins.listToAttrs (map (group: {
    name = group;
    value.hosts = map (host: host.name) (builtins.filter (host: host.group == group) fleetHosts);
  }) groupNames);
}
