//! Vocabularies for text columns. Small on purpose: plausible-looking values matter for
//! dashboards and query plans, not linguistic variety.

pub const FIRST_NAMES: &[&str] = &[
    "Giuseppe", "Maria", "Giovanni", "Anna", "Antonio", "Giuseppina", "Mario", "Rosa", "Luigi", "Angela", "Francesco", "Giovanna",
    "Angelo", "Teresa", "Vincenzo", "Lucia", "Pietro", "Carmela", "Salvatore", "Caterina", "Carlo", "Francesca", "Franco", "Elena",
    "Paolo", "Rita", "Marco", "Laura", "Andrea", "Chiara", "Luca", "Sara", "Matteo", "Giulia", "Davide", "Valentina", "Alessandro",
    "Martina", "Lorenzo", "Silvia", "James", "Emma", "Liam", "Olivia", "Noah", "Sophia", "Lukas", "Hannah", "Pierre", "Camille",
];

pub const LAST_NAMES: &[&str] = &[
    "Rossi", "Russo", "Ferrari", "Esposito", "Bianchi", "Romano", "Colombo", "Ricci", "Marino", "Greco", "Bruno", "Gallo", "Conti",
    "De Luca", "Mancini", "Costa", "Giordano", "Rizzo", "Lombardi", "Moretti", "Barbieri", "Fontana", "Santoro", "Mariani",
    "Rinaldi", "Caruso", "Ferrara", "Galli", "Martini", "Leone", "Longo", "Gentile", "Martinelli", "Vitale", "Serra", "Coppola",
    "De Santis", "D'Angelo", "Marchetti", "Parisi", "Villa", "Conte", "Smith", "Johnson", "Brown", "Müller", "Schmidt", "Martin",
    "Bernard", "García",
];

pub const CITIES: &[&str] = &[
    "Roma", "Milano", "Napoli", "Torino", "Palermo", "Genova", "Bologna", "Firenze", "Bari", "Catania", "Venezia", "Verona",
    "Messina", "Padova", "Trieste", "Brescia", "Parma", "Modena", "Reggio Emilia", "Perugia", "Livorno", "Cagliari", "Paris",
    "London", "Berlin", "Madrid", "Vienna", "Zürich", "Lyon", "München",
];

pub const COUNTRIES: &[&str] = &[
    "Italia", "France", "Deutschland", "España", "Österreich", "Schweiz", "United Kingdom", "Portugal", "România", "Shqipëria",
    "Ukraine", "Polska", "Nederland", "Belgique", "Sverige",
];

pub const STREETS: &[&str] = &[
    "Via Roma", "Via Garibaldi", "Via Mazzini", "Via Cavour", "Corso Italia", "Via Dante", "Via Verdi", "Piazza della Repubblica",
    "Via Manzoni", "Viale Europa", "Via XX Settembre", "Via Marconi", "Via Matteotti", "Via Vittorio Veneto", "Via San Martino",
];

pub const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "north", "south", "east", "west", "central", "main", "service", "system", "network", "device",
    "sensor", "report", "update", "check", "event", "record", "status", "level", "control", "monitor", "signal", "value", "range",
    "limit", "change", "request", "response", "process", "task", "batch", "queue", "cache", "index", "stream", "window", "frame",
    "block", "unit", "group", "team", "room", "floor", "area", "zone", "point", "line", "path", "route", "node", "link", "port",
    "channel", "source", "target", "input", "output", "reading", "alert", "notice", "detail", "summary", "note", "entry", "item",
];

pub const SENTENCE_STARTS: &[&str] = &[
    "Checked", "Reported", "Updated", "Detected", "Confirmed", "Scheduled", "Resolved", "Observed", "Recorded", "Requested",
];
