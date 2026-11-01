# The CAD outputs, all written to build/cad/:
#
#   frontplate  the STEP of the front plate, its DXF for cutting, a DXF of
#               the plate for each engraved curve, and a DXF and a PNG of a
#               sampler of nine of the curves. They need no model. With the
#               STEP model of the boards, the plate is first checked against
#               it, and the STEP of the boards with the plate is written too.
#   render      five PNG views, with a transparent background, of the model.
#   readme      copies the PNGs that the README shows to docs/img/.
#
# The model is not in the repository; point STEP= at it if it is elsewhere.
# render also renders another STEP, such as the assembly:
# make -C cad render STEP=../build/cad/Knixx_Thunder_00_assembly.step.
# The Python packages are installed once in a venv here.

STEP   ?= ../pcb/Knixx_Thunder_00.step
OUTDIR ?= ../build/cad
DOCS   := ../docs/img
VENV   := .venv
PYTHON := $(VENV)/bin/python
# cadquery-ocp has wheels for Python 3.11 to 3.14 only; take the newest of
# those that is installed. Override with PY=.
PY     ?= $(firstword $(foreach v,14 13 12 11,$(shell command -v python3.$(v))) python3)

# The outputs are named after the model, also when it is absent.
NAME  := $(basename $(notdir $(STEP)))
MODEL := $(wildcard $(STEP))

# One run of a script writes all its outputs; a stamp per run stands for
# them, as GNU make 3.81 has no grouped targets.
RENDER_STAMP := $(OUTDIR)/.render-$(NAME)
PLATE_STAMP  := $(OUTDIR)/.frontplate-$(NAME)
README_PNGS  := $(NAME)_iso_front.png $(NAME)_iso_back.png $(NAME)_side.png \
                $(NAME)_sampler.png

.PHONY: render frontplate readme clean

render: $(RENDER_STAMP)

$(RENDER_STAMP): $(MODEL) render_step.py $(VENV)/.installed
	@test -n "$(MODEL)" || { echo "render: no STEP model at $(STEP); set STEP=" >&2; exit 1; }
	@mkdir -p $(OUTDIR)
	$(PYTHON) render_step.py $(MODEL) $(OUTDIR)
	@touch $@

# The model, when present, is a prerequisite: the assembly is written once
# it appears.
frontplate: $(PLATE_STAMP)

$(PLATE_STAMP): $(MODEL) frontplate.py $(VENV)/.installed
	@mkdir -p $(OUTDIR)
	$(PYTHON) frontplate.py $(OUTDIR) $(NAME) $(MODEL)
	@touch $@

readme: $(RENDER_STAMP) $(PLATE_STAMP)
	@mkdir -p $(DOCS)
	cp $(addprefix $(OUTDIR)/,$(README_PNGS)) $(DOCS)/

$(VENV)/.installed: requirements.txt
	rm -rf $(VENV)
	$(PY) -m venv $(VENV)
	$(PYTHON) -m pip install -q -r requirements.txt
	touch $@

clean:
	rm -rf $(VENV)
