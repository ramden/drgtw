"""Synthetic labelled-sample generation with Faker (de_DE + en_US).

Templates mix a person name, a company and a city into realistic sentences.
Each template is a format string with the placeholders ``{name}``,
``{company}`` and ``{city}`` (a template may use any subset). We render by
substituting one slot at a time so the *character* offset of each inserted
value is known exactly — the emitted entity spans satisfy
``text[start:end] == value``.

``generate(n, seed)`` is fully deterministic: same ``(n, seed)`` -> identical
samples, including which locale/template/values were chosen.
"""

from __future__ import annotations

import random
from dataclasses import dataclass

from faker import Faker

from .data import Entity, Sample

# Placeholder -> entity label mapping.
_SLOT_LABEL = {"name": "PER", "company": "ORG", "city": "LOC"}


@dataclass(frozen=True)
class _Template:
    text: str  # contains {name}/{company}/{city} placeholders


# 10+ German templates.
_DE_TEMPLATES: list[_Template] = [
    _Template("Schreib eine Mail an {name} von der {company} in {city}."),
    _Template("{name} arbeitet bei {company} und wohnt in {city}."),
    _Template("Bitte ruf {name} bei der {company} zurück."),
    _Template("Die {company} hat ihren Sitz in {city}."),
    _Template("Kannst du {name} aus {city} kontaktieren?"),
    _Template("Der Vertrag mit {company} wurde von {name} unterzeichnet."),
    _Template("{name} reist nächste Woche nach {city}."),
    _Template("Unser Ansprechpartner bei {company} ist {name}."),
    _Template("Treffen mit {name} in {city} am Montag."),
    _Template("{company} sucht eine neue Niederlassung in {city}."),
    _Template("Frau {name} leitet das Büro in {city}."),
    _Template("Ich habe {name} von {company} gestern getroffen."),
]

# 10+ English templates.
_EN_TEMPLATES: list[_Template] = [
    _Template("Write an email to {name} from {company} in {city}."),
    _Template("{name} works at {company} and lives in {city}."),
    _Template("Please call {name} back at {company}."),
    _Template("{company} is headquartered in {city}."),
    _Template("Could you reach out to {name} from {city}?"),
    _Template("The contract with {company} was signed by {name}."),
    _Template("{name} is travelling to {city} next week."),
    _Template("Our contact at {company} is {name}."),
    _Template("Meeting with {name} in {city} on Monday."),
    _Template("{company} is opening a new office in {city}."),
    _Template("{name} manages the {city} branch."),
    _Template("I met {name} from {company} yesterday."),
]

_LOCALES = ("de_DE", "en_US")


def _render(template: _Template, values: dict[str, str]) -> Sample:
    """Render a template, computing exact character offsets for each slot.

    We replace placeholders left-to-right, tracking the running output so each
    inserted value's start offset is the current length of the output prefix.
    """
    import re

    out: list[str] = []
    entities: list[Entity] = []
    pos = 0  # character length of the rendered prefix so far
    text = template.text
    last = 0
    for m in re.finditer(r"\{(name|company|city)\}", text):
        literal = text[last : m.start()]
        out.append(literal)
        pos += len(literal)
        slot = m.group(1)
        value = values[slot]
        start = pos
        out.append(value)
        pos += len(value)
        end = pos
        entities.append(Entity(start=start, end=end, label=_SLOT_LABEL[slot]))
        last = m.end()
    tail = text[last:]
    out.append(tail)
    rendered = "".join(out)
    # Sort entities by start so the Sample is in canonical order.
    entities.sort(key=lambda e: e.start)
    return Sample(text=rendered, entities=entities)


def generate(n: int, seed: int = 0) -> list[Sample]:
    """Generate ``n`` synthetic samples deterministically.

    Half (rounded) German, half English; locale/template/value selection is
    driven by a single seeded RNG so results are reproducible.
    """
    if n < 0:
        raise ValueError("n must be >= 0")
    rng = random.Random(seed)
    # Faker seeding is global; set it from our RNG-derived seed so the whole
    # generation is reproducible from the single `seed` argument.
    Faker.seed(seed)
    fakers = {loc: Faker(loc) for loc in _LOCALES}

    samples: list[Sample] = []
    for _ in range(n):
        locale = rng.choice(_LOCALES)
        fake = fakers[locale]
        templates = _DE_TEMPLATES if locale == "de_DE" else _EN_TEMPLATES
        template = rng.choice(templates)
        values = {
            "name": fake.name(),
            "company": fake.company(),
            "city": fake.city(),
        }
        samples.append(_render(template, values))
    return samples
