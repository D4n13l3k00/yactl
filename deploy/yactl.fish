# yactl keeps its executable and state in one private directory.
if not contains -- /data/.yactl $PATH
    set -gx PATH /data/.yactl $PATH
end
