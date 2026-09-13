[out:xml][timeout:120];
(
  way["highway"](21.32,-158.00,21.39,-157.85);
  way["building"](21.32,-158.00,21.39,-157.85);
  way["natural"="water"](21.32,-158.00,21.39,-157.85);
  way["waterway"~"^(riverbank|river|canal|dock)$"](21.32,-158.00,21.39,-157.85);
  way["landuse"~"^(park|forest|grass|residential|military|farmland|cemetery|industrial|commercial)$"](21.32,-158.00,21.39,-157.85);
  way["leisure"~"^(park|golf_course|pitch|stadium)$"](21.32,-158.00,21.39,-157.85);
  way["natural"~"^(wood|scrub|beach|sand|wetland)$"](21.32,-158.00,21.39,-157.85);
  way["aeroway"~"^(runway|taxiway|apron)$"](21.32,-158.00,21.39,-157.85);
  way["railway"~"^(rail|light_rail|tram|subway)$"](21.32,-158.00,21.39,-157.85);
  relation["natural"="water"](21.32,-158.00,21.39,-157.85);
  relation["landuse"](21.32,-158.00,21.39,-157.85);
  relation["leisure"](21.32,-158.00,21.39,-157.85);
);
(._;>;);
out body;
