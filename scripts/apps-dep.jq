[.packages[]
 | select(.name != "printkit")
 | .dependencies[]
 | select(.path != null)
 | select((.path | split("\\") | join("/")) | contains("apps/"))
 | .name] | join(",")
