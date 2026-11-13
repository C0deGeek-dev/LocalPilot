extends Node2D
## A red square that falls onto a floor and moves with the `move_right` action.
## `--scenario run` (after `--`) presses `move_right` for the first 30 physics
## frames, prints one STATE line at frame 60 and quits; `--shot <file>` also
## saves the rendered viewport.

const FLOOR_Y := 140.0
const SIZE := 32.0
const SPEED := 120.0
const GRAVITY := 900.0

var vel_y := 0.0
var frames := 0
var scenario := ""
var shot := ""
@onready var player: ColorRect = $Player


func _ready() -> void:
	var args := OS.get_cmdline_user_args()
	for i in args.size():
		if args[i] == "--scenario" and i + 1 < args.size():
			scenario = args[i + 1]
		if args[i] == "--shot" and i + 1 < args.size():
			shot = args[i + 1]
	for binding in [["move_left", KEY_LEFT], ["move_right", KEY_RIGHT]]:
		InputMap.add_action(binding[0])
		var key := InputEventKey.new()
		key.physical_keycode = binding[1]
		InputMap.action_add_event(binding[0], key)
	player.position = Vector2(20, 20)


func _physics_process(delta: float) -> void:
	frames += 1
	if scenario == "run":
		if frames == 1:
			Input.action_press("move_right")
		if frames == 31:
			Input.action_release("move_right")
	var move := Input.get_axis("move_left", "move_right")
	player.position.x += move * SPEED * delta
	vel_y += GRAVITY * delta
	player.position.y += vel_y * delta
	# Land on the floor.
	if player.position.y + SIZE >= FLOOR_Y:
		player.position.y = FLOOR_Y - SIZE
		vel_y = 0.0
	if scenario == "run" and frames == 60:
		print("STATE ", JSON.stringify({"x": snappedf(player.position.x, 0.01), "y": snappedf(player.position.y, 0.01), "frames": frames}))
		if shot != "":
			await RenderingServer.frame_post_draw
			get_viewport().get_texture().get_image().save_png(shot)
			print("SHOT ", shot)
		get_tree().quit()
