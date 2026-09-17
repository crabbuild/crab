def gib: 1073741824;

def between($value; $minimum; $maximum):
  $value >= ($minimum * gib) and $value <= ($maximum * gib);

def matches_profile($profile):
  .resources as $resources |
  if $profile == "small" then
    ($resources.job_credits >= 1 and $resources.job_credits <= 2 and
     between($resources.memory_bytes; 2; 4) and
     between($resources.disk_capacity_bytes; 50; 100))
  elif $profile == "medium" then
    ($resources.job_credits >= 4 and $resources.job_credits <= 8 and
     between($resources.memory_bytes; 8; 16) and
     between($resources.disk_capacity_bytes; 100; 200))
  elif $profile == "large" then
    ($resources.job_credits == 16 and
     between($resources.memory_bytes; 32; 64) and
     between($resources.disk_capacity_bytes; 500; 1000))
  else
    false
  end;

type == "array" and
length >= 3 and
all(.[].envelope;
  (.admission.active_cells >= 1000 and .admission.active_cells <= 10000) and
  (.resources.disk_limit_bytes == $disk_limit_bytes) and
  matches_profile($profile))
