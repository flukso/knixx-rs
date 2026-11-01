/*
 * Runs the ST ULD driver through the configuration that knixx-vl53l7cx
 * applies, against a fake sensor, and logs every I2C transaction and wait.
 * The log is the reference that the Rust port is compared with, see
 * src/tests.rs, which implements the same fake sensor.
 *
 * Log lines:
 *   W <register> <length> <FNV-1a 32 of the data>   write
 *   R <register> <length>                           read
 *   D <ms>                                          wait
 *   M <motion[0]> ... <motion[31]>                  parsed motion indicator
 */
#include <stdio.h>
#include <stdlib.h>

#include "vl53l7cx_api.h"
#include "vl53l7cx_plugin_detection_thresholds.h"
#include "vl53l7cx_plugin_motion_indicator.h"

/* Size of a ranging data frame with only the motion indicator enabled. */
#define FRAME_SIZE 196

static uint32_t fnv(const uint8_t *p, uint32_t n)
{
	uint32_t h = 2166136261u;
	for (uint32_t i = 0; i < n; i++) {
		h ^= p[i];
		h *= 16777619u;
	}
	return h;
}

static void put32(uint8_t *p, uint32_t v)
{
	p[0] = v >> 24; p[1] = v >> 16; p[2] = v >> 8; p[3] = v;
}

/* frame builds the ranging data frame the fake sensor returns, in the
 * sensor's big endian word order. */
static void frame(uint8_t *raw)
{
	memset(raw, 0, FRAME_SIZE);
	raw[0] = 7;                       /* stream count */
	raw[10] = 0x12; raw[11] = 0x34;   /* header id */
	put32(raw + 16, 0x54B400C0u);     /* metadata block */
	put32(raw + 32, 0x54C00040u);     /* common data block */
	put32(raw + 40, 0xD85808C0u);     /* motion indicator block */
	put32(raw + 44, 1111);            /* global_indicator_1 */
	put32(raw + 48, 2222);            /* global_indicator_2 */
	raw[52 + 3] = 1;                  /* status */
	raw[52 + 2] = 3;                  /* nb_of_detected_aggregates */
	raw[52 + 1] = 16;                 /* nb_of_aggregates */
	for (uint32_t k = 0; k < 32; k++)
		put32(raw + 56 + 4 * k, (k + 1) * 10 * 65535u);
	raw[FRAME_SIZE - 2] = 0x12; raw[FRAME_SIZE - 1] = 0x34; /* footer id */
}

/* reply fills a read of size bytes from register reg. */
static void reply(uint16_t reg, uint8_t *p, uint32_t size)
{
	static const uint8_t status[] = {0x02, 0x03, 0x00, 0x00};

	for (uint32_t i = 0; i < size; i++)
		p[i] = (uint8_t)(i * 7 + 3);

	switch (reg) {
	case 0x0000:
		if (size == 1)
			p[0] = 0xF0;      /* device id */
		else if (size == FRAME_SIZE)
			frame(p);
		break;
	case 0x0001:
		p[0] = 0x02;              /* revision id */
		break;
	case 0x0006:
		p[0] = 0x01;              /* MCU booted */
		break;
	case 0x0007:
		p[0] = 0x00;
		break;
	case 0x0021:
		p[0] = 0x10;              /* firmware access ready */
		break;
	case 0x2FFC:
		/* Auto stop flag: the firmware already stopped, so that
		 * stop_ranging does not wait for the MCU. */
		p[0] = 0xFF; p[1] = 0x04; p[2] = 0x00; p[3] = 0x00;
		break;
	case 0x2C00:                      /* UI command status */
		for (uint32_t i = 0; i < size; i++)
			p[i] = i < 4 ? status[i] : 0;
		break;
	case 0x2C04:
		/* The output data size, read back by start_ranging. */
		if (size == 24) {
			p[14] = FRAME_SIZE >> 8;
			p[15] = FRAME_SIZE & 0xFF;
		}
		break;
	default:
		if (size == 1)
			p[0] = 0;
	}
}

uint8_t VL53L7CX_RdByte(VL53L7CX_Platform *p_platform, uint16_t reg,
		uint8_t *p_value)
{
	return VL53L7CX_RdMulti(p_platform, reg, p_value, 1);
}

uint8_t VL53L7CX_WrByte(VL53L7CX_Platform *p_platform, uint16_t reg,
		uint8_t value)
{
	return VL53L7CX_WrMulti(p_platform, reg, &value, 1);
}

uint8_t VL53L7CX_RdMulti(VL53L7CX_Platform *p_platform, uint16_t reg,
		uint8_t *p_values, uint32_t size)
{
	(void)p_platform;
	printf("R %04x %u\n", reg, size);
	reply(reg, p_values, size);
	return 0;
}

uint8_t VL53L7CX_WrMulti(VL53L7CX_Platform *p_platform, uint16_t reg,
		uint8_t *p_values, uint32_t size)
{
	(void)p_platform;
	printf("W %04x %u %08x\n", reg, size, fnv(p_values, size));
	return 0;
}

void VL53L7CX_SwapBuffer(uint8_t *buffer, uint16_t size)
{
	uint32_t i, tmp;

	for (i = 0; i < size; i = i + 4) {
		tmp = (buffer[i] << 24) | (buffer[i + 1] << 16)
			| (buffer[i + 2] << 8) | (buffer[i + 3]);
		memcpy(&(buffer[i]), &tmp, 4);
	}
}

uint8_t VL53L7CX_WaitMs(VL53L7CX_Platform *p_platform, uint32_t TimeMs)
{
	(void)p_platform;
	printf("D %u\n", TimeMs);
	return 0;
}

static void check(const char *step, uint8_t status)
{
	if (status != 0) {
		fprintf(stderr, "%s: status %u\n", step, status);
		exit(1);
	}
}

int main(void)
{
	static VL53L7CX_Configuration dev;
	static VL53L7CX_Motion_Configuration motion;
	static VL53L7CX_DetectionThresholds thresholds[VL53L7CX_NB_THRESHOLDS];
	static VL53L7CX_ResultsData results;
	uint8_t alive = 0;

	dev.platform.address = VL53L7CX_DEFAULT_I2C_ADDRESS;

	check("is_alive", vl53l7cx_is_alive(&dev, &alive));
	if (!alive)
		check("is_alive", 1);
	check("init", vl53l7cx_init(&dev));
	check("resolution", vl53l7cx_set_resolution(&dev, VL53L7CX_RESOLUTION_4X4));
	check("frequency", vl53l7cx_set_ranging_frequency_hz(&dev, 1));
	check("mode", vl53l7cx_set_ranging_mode(&dev, VL53L7CX_RANGING_MODE_AUTONOMOUS));
	check("integration", vl53l7cx_set_integration_time_ms(&dev, 5));
	check("motion", vl53l7cx_motion_indicator_init(&dev, &motion,
		VL53L7CX_RESOLUTION_4X4));
	check("distance", vl53l7cx_motion_indicator_set_distance_motion(&dev,
		&motion, 1000, 2400));

	for (uint8_t i = 0; i < 16; i++) {
		thresholds[i].zone_num = i;
		thresholds[i].measurement = VL53L7CX_MOTION_INDICATOR;
		thresholds[i].type = VL53L7CX_GREATER_THAN_MAX_CHECKER;
		thresholds[i].mathematic_operation = VL53L7CX_OPERATION_NONE;
		thresholds[i].param_low_thresh = 44;
		thresholds[i].param_high_thresh = 44;
	}
	thresholds[15].zone_num |= VL53L7CX_LAST_THRESHOLD;
	check("thresholds", vl53l7cx_set_detection_thresholds(&dev, thresholds));
	check("enable", vl53l7cx_set_detection_thresholds_enable(&dev, 1));
	check("start", vl53l7cx_start_ranging(&dev));

	check("data", vl53l7cx_get_ranging_data(&dev, &results));
	printf("M");
	for (int k = 0; k < 32; k++)
		printf(" %u", results.motion_indicator.motion[k]);
	printf("\n");

	check("stop", vl53l7cx_stop_ranging(&dev));
	return 0;
}
