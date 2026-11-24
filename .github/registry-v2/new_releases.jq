# The published Sunrise releases carrying an SDK descriptor that the judging job has not
# considered, one "<tag> <target>" per line. Input: GitHub's release list. $considered: the
# stored considered.json. A rolling tag that moved to another commit is a new release.
($considered[0] | map([.tag, .target])) as $seen
| .[]
| select((.draft | not) and any(.assets[]; .name == "sdk.json"))
| [.tag_name, .target_commitish]
| select(. as $release | $seen | index([$release]) | not)
| join(" ")
