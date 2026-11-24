# Mod policy

> **Draft: umceko has not reviewed this yet.**

Sunrise's registry lists mods, modpacks, resource packs, data packs and shader packs made for Sunrise. This page
says who answers for them and when one is taken down.

## Who answers for a project

A project belongs to its authors, and the owner of its registered repository answers for it. Each project is under the licence its
manifest names, and the registry accepts non-commercial licences (such as CC-BY-NC-SA-4.0), because listing and
installing are free. Sunrise reviews each release before listing it, but listing is not an endorsement, and the review is
no guarantee.

## What an author promises

When you submit a project, you promise that:

- you have the right to publish everything in it, under the licence you name;
- it does what its page says, and nothing hidden: no malware, no collecting or sending players' data they
  weren't told about, no ads or payments built into the game;
- it contains no Minecraft game files or assets, and no tables or values generated from them: a mod reads such
  values from the game at runtime through the mod API;
- it contains nothing that breaks the
  [Minecraft EULA](https://www.minecraft.net/eula) or [Usage Guidelines](https://www.minecraft.net/usage-guidelines);
- it contains nothing illegal, hateful or sexual;
- you will keep its page accurate.

## Takedowns

A file or a whole project is taken down by adding it to [`registry/revoked.toml`](registry/revoked.toml), with the reason. The
launcher then refuses to install it, and turns off copies already installed, showing the reason. Sunrise takes a
project down when it breaks this policy, when there is a valid legal request, or when its owners ask.

To ask for a takedown, open a [takedown issue](https://github.com/sunrise-org/sunrise-mods/issues/new?template=takedown.yml).
For a private matter, contact the Sunrise core team on [Discord](https://playsunrise.net).

This policy may change. Changes are made by pull request in this repository.
