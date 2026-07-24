// pmb3 — the pm-shaped seam over Box3D. pm code NEVER sees a b3* type:
// this file is the only translation unit that includes box3d headers,
// and it exposes a primitive-typed API (floats, out-pointers, packed
// u32/u64 ids) the Rust side declares by hand. Two reasons this is a
// shim and not bindgen output:
//   1. no libclang anywhere in the build (Linux, native Windows,
//      windows-gnu cross all just need a C compiler cc already finds);
//   2. Box3D is alpha — an upstream API change lands HERE, shim-sized,
//      instead of rippling b3* types through game code.
// Spike-1 surface only: worlds, static/dynamic boxes, step, poses,
// sleep state. Capsules, filters, contacts, joints, the mover, and the
// recording/keyframe machinery come with their spikes.

#include "box3d/box3d.h"
#include <stdint.h>

typedef struct
{
	float x, y, z;
} PmbVec3;

typedef struct
{
	float x, y, z, w;
} PmbQuat;

// Ids pack the b3 handle structs into plain integers so Rust treats
// them as opaque numbers: world = index1 | generation<<16; body =
// index1 | world0<<32 | generation<<48.
static b3WorldId pmb3_unpack_world( uint32_t w )
{
	b3WorldId id = { (uint16_t)( w & 0xFFFF ), (uint16_t)( w >> 16 ) };
	return id;
}

static uint64_t pmb3_pack_body( b3BodyId id )
{
	return (uint64_t)(uint32_t)id.index1 | ( (uint64_t)id.world0 << 32 ) | ( (uint64_t)id.generation << 48 );
}

static b3BodyId pmb3_unpack_body( uint64_t v )
{
	b3BodyId id = { (int32_t)(uint32_t)( v & 0xFFFFFFFF ), (uint16_t)( ( v >> 32 ) & 0xFFFF ), (uint16_t)( v >> 48 ) };
	return id;
}

uint32_t pmb3_world_create( float gx, float gy, float gz )
{
	b3WorldDef def = b3DefaultWorldDef();
	def.gravity = ( b3Vec3 ){ gx, gy, gz };
	b3WorldId id = b3CreateWorld( &def );
	return (uint32_t)id.index1 | ( (uint32_t)id.generation << 16 );
}

void pmb3_world_destroy( uint32_t w )
{
	b3DestroyWorld( pmb3_unpack_world( w ) );
}

void pmb3_world_step( uint32_t w, float dt, int substeps )
{
	b3World_Step( pmb3_unpack_world( w ), dt, substeps );
}

// One creation door for spike 1: a box body. type: 0 = static,
// 1 = kinematic, 2 = dynamic (matches b3BodyType by construction,
// asserted below so an upstream enum shuffle is a compile error).
_Static_assert( b3_staticBody == 0 && b3_kinematicBody == 1 && b3_dynamicBody == 2, "pmb3 type map" );

uint64_t pmb3_body_box( uint32_t w, int type, PmbVec3 pos, PmbQuat rot, PmbVec3 half, float density, float friction )
{
	b3BodyDef bd = b3DefaultBodyDef();
	bd.type = (b3BodyType)type;
	bd.position = ( b3Pos ){ pos.x, pos.y, pos.z };
	bd.rotation = ( b3Quat ){ .v = { rot.x, rot.y, rot.z }, .s = rot.w };
	b3BodyId body = b3CreateBody( pmb3_unpack_world( w ), &bd );
	b3ShapeDef sd = b3DefaultShapeDef();
	sd.density = density;
	sd.baseMaterial.friction = friction;
	b3BoxHull box = b3MakeBoxHull( half.x, half.y, half.z );
	b3CreateHullShape( body, &sd, &box.base );
	return pmb3_pack_body( body );
}

// Upright capsule (axis = local y, hemisphere centers at ±half_h):
// the hog shape. lock_upright freezes angular x/z so the critter
// jostles and yaws but never tips — the character-crowd idiom.
uint64_t pmb3_body_capsule( uint32_t w, int type, PmbVec3 pos, float half_h, float radius, float density,
							float friction, int lock_upright )
{
	b3BodyDef bd = b3DefaultBodyDef();
	bd.type = (b3BodyType)type;
	bd.position = ( b3Pos ){ pos.x, pos.y, pos.z };
	if ( lock_upright )
	{
		bd.motionLocks.angularX = true;
		bd.motionLocks.angularZ = true;
	}
	b3BodyId body = b3CreateBody( pmb3_unpack_world( w ), &bd );
	b3ShapeDef sd = b3DefaultShapeDef();
	sd.density = density;
	sd.baseMaterial.friction = friction;
	b3Capsule capsule = { { 0.0f, -half_h, 0.0f }, { 0.0f, half_h, 0.0f }, radius };
	b3CreateCapsuleShape( body, &sd, &capsule );
	return pmb3_pack_body( body );
}

void pmb3_body_set_velocity( uint64_t body, PmbVec3 v )
{
	b3Body_SetLinearVelocity( pmb3_unpack_body( body ), ( b3Vec3 ){ v.x, v.y, v.z } );
}

void pmb3_body_force( uint64_t body, PmbVec3 f )
{
	b3Body_ApplyForceToCenter( pmb3_unpack_body( body ), ( b3Vec3 ){ f.x, f.y, f.z }, true );
}

void pmb3_body_set_damping( uint64_t body, float linear )
{
	b3Body_SetLinearDamping( pmb3_unpack_body( body ), linear );
}

void pmb3_body_lock_rotation( uint64_t body )
{
	b3MotionLocks locks = { false, false, false, true, true, true };
	b3Body_SetMotionLocks( pmb3_unpack_body( body ), locks );
}

// Convex hull body from raw points (≤ 64, world/local space of the
// body). The shape clones the hull data, so the temporary is freed
// here. Ramps and any authored convex chunk come through this door.
uint64_t pmb3_body_hull( uint32_t w, int type, PmbVec3 pos, PmbQuat rot, const PmbVec3* pts, int n, float density,
						 float friction )
{
	b3BodyDef bd = b3DefaultBodyDef();
	bd.type = (b3BodyType)type;
	bd.position = ( b3Pos ){ pos.x, pos.y, pos.z };
	bd.rotation = ( b3Quat ){ .v = { rot.x, rot.y, rot.z }, .s = rot.w };
	b3BodyId body = b3CreateBody( pmb3_unpack_world( w ), &bd );
	b3ShapeDef sd = b3DefaultShapeDef();
	sd.density = density;
	sd.baseMaterial.friction = friction;
	b3HullData* hull = b3CreateHull( (const b3Vec3*)pts, n, 64 );
	if ( hull != NULL )
	{
		b3CreateHullShape( body, &sd, hull );
		b3DestroyHull( hull );
	}
	return pmb3_pack_body( body );
}

void pmb3_body_destroy( uint64_t body )
{
	b3DestroyBody( pmb3_unpack_body( body ) );
}

// Teleport (kinematic mirrors, respawn resets) — not for regular
// motion, which should be velocities/forces.
void pmb3_body_set_pose( uint64_t body, PmbVec3 pos, PmbQuat rot )
{
	b3Body_SetTransform( pmb3_unpack_body( body ), ( b3Pos ){ pos.x, pos.y, pos.z },
						 ( b3Quat ){ .v = { rot.x, rot.y, rot.z }, .s = rot.w } );
}

uint64_t pmb3_body_sphere( uint32_t w, int type, PmbVec3 pos, float radius, float density, float friction )
{
	b3BodyDef bd = b3DefaultBodyDef();
	bd.type = (b3BodyType)type;
	bd.position = ( b3Pos ){ pos.x, pos.y, pos.z };
	b3BodyId body = b3CreateBody( pmb3_unpack_world( w ), &bd );
	b3ShapeDef sd = b3DefaultShapeDef();
	sd.density = density;
	sd.baseMaterial.friction = friction;
	b3Sphere sphere = { { 0.0f, 0.0f, 0.0f }, radius };
	b3CreateSphereShape( body, &sd, &sphere );
	return pmb3_pack_body( body );
}

void pmb3_body_set_angular_velocity( uint64_t body, PmbVec3 v )
{
	b3Body_SetAngularVelocity( pmb3_unpack_body( body ), ( b3Vec3 ){ v.x, v.y, v.z } );
}

void pmb3_body_angular_velocity( uint64_t body, PmbVec3* v )
{
	b3Vec3 a = b3Body_GetAngularVelocity( pmb3_unpack_body( body ) );
	v->x = a.x;
	v->y = a.y;
	v->z = a.z;
}

// Wheel joint for a y-up, +z-forward vehicle, axle on x. Box3D's
// conventions (read from wheel_joint.c): suspension slides along joint
// frame A's local X, the wheel spins about frame B's local Z — so
// frame A rotates x̂→ŷ (about z by +90°) and frame B rotates ẑ→x̂
// (about y by +90°). The shim bakes those quats so callers just hand
// the chassis-space mount point. Returns a packed joint id; steering
// stays fixed-forward here (the audition drives by spin + our forces).
uint64_t pmb3_wheel_joint( uint32_t w, uint64_t chassis, uint64_t wheel, PmbVec3 mount, float hertz, float damping,
						   float max_torque )
{
	const float s = 0.70710678f; // sin/cos 45° — the two 90° quats
	b3WheelJointDef def = b3DefaultWheelJointDef();
	def.base.bodyIdA = pmb3_unpack_body( chassis );
	def.base.bodyIdB = pmb3_unpack_body( wheel );
	def.base.localFrameA.p = ( b3Vec3 ){ mount.x, mount.y, mount.z };
	def.base.localFrameA.q = ( b3Quat ){ .v = { 0.0f, 0.0f, s }, .s = s }; // x̂→ŷ
	def.base.localFrameB.p = ( b3Vec3 ){ 0.0f, 0.0f, 0.0f };
	def.base.localFrameB.q = ( b3Quat ){ .v = { 0.0f, s, 0.0f }, .s = s }; // ẑ→x̂
	def.enableSuspensionSpring = true;
	def.suspensionHertz = hertz;
	def.suspensionDampingRatio = damping;
	def.enableSpinMotor = true;
	def.maxSpinTorque = max_torque;
	def.spinSpeed = 0.0f;
	b3JointId id = b3CreateWheelJoint( pmb3_unpack_world( w ), &def );
	return (uint64_t)(uint32_t)id.index1 | ( (uint64_t)id.world0 << 32 ) | ( (uint64_t)id.generation << 48 );
}

void pmb3_wheel_spin( uint64_t joint, float speed )
{
	b3JointId id = { (int32_t)(uint32_t)( joint & 0xFFFFFFFF ), (uint16_t)( ( joint >> 32 ) & 0xFFFF ),
					 (uint16_t)( joint >> 48 ) };
	b3WheelJoint_SetSpinMotorSpeed( id, speed );
}

void pmb3_body_pose( uint64_t body, PmbVec3* pos, PmbQuat* rot )
{
	b3BodyId id = pmb3_unpack_body( body );
	b3Pos p = b3Body_GetPosition( id );
	b3Quat q = b3Body_GetRotation( id );
	pos->x = (float)p.x;
	pos->y = (float)p.y;
	pos->z = (float)p.z;
	// b3Quat is {v: xyz, s: w} — pm's (x, y, z, w) order on the way out.
	rot->x = q.v.x;
	rot->y = q.v.y;
	rot->z = q.v.z;
	rot->w = q.s;
}

void pmb3_body_velocity( uint64_t body, PmbVec3* vel )
{
	b3Vec3 v = b3Body_GetLinearVelocity( pmb3_unpack_body( body ) );
	vel->x = v.x;
	vel->y = v.y;
	vel->z = v.z;
}

int pmb3_body_awake( uint64_t body )
{
	return b3Body_IsAwake( pmb3_unpack_body( body ) ) ? 1 : 0;
}

// --- queries + runtime shape tweaks (the collisions-on-the-solver
// slice, 2026-07-23). Same seam rule: primitive types only.

// Every shape a body owns (hitbox bodies are single-shape today; the
// cap is defensive).
#define PMB3_MAX_SHAPES 8

void pmb3_body_set_friction( uint64_t body, float mu )
{
	b3BodyId id = pmb3_unpack_body( body );
	b3ShapeId shapes[PMB3_MAX_SHAPES];
	int n = b3Body_GetShapes( id, shapes, PMB3_MAX_SHAPES );
	for ( int i = 0; i < n; ++i )
	{
		b3Shape_SetFriction( shapes[i], mu );
	}
	// New friction only reaches contacts made after the change; waking
	// the body refreshes a sleeping stack's manifolds promptly.
	b3Body_SetAwake( id, true );
}

// Category/mask on every shape of a body (b3Filter semantics: contact
// iff a.category & b.mask AND b.category & a.mask; queries use the
// same test against a b3QueryFilter).
void pmb3_body_set_filter( uint64_t body, uint64_t category, uint64_t mask )
{
	b3BodyId id = pmb3_unpack_body( body );
	b3ShapeId shapes[PMB3_MAX_SHAPES];
	int n = b3Body_GetShapes( id, shapes, PMB3_MAX_SHAPES );
	b3Filter filter = b3DefaultFilter();
	filter.categoryBits = category;
	filter.maskBits = mask;
	for ( int i = 0; i < n; ++i )
	{
		b3Shape_SetFilter( shapes[i], filter, true );
	}
}

void pmb3_body_set_type( uint64_t body, int type )
{
	b3Body_SetType( pmb3_unpack_body( body ), (b3BodyType)type );
}

// Closest hit in the live world against shapes whose category is in
// `mask` (statics for bullet/wall clipping). Returns 0 on miss.
int pmb3_world_cast_ray( uint32_t w, PmbVec3 origin, PmbVec3 translation, uint64_t mask, PmbVec3* point,
						 float* frac )
{
	b3QueryFilter filter = b3DefaultQueryFilter();
	filter.categoryBits = ~0ull;
	filter.maskBits = mask;
	b3RayResult r = b3World_CastRayClosest( pmb3_unpack_world( w ), ( b3Pos ){ origin.x, origin.y, origin.z },
											( b3Vec3 ){ translation.x, translation.y, translation.z }, filter );
	if ( !r.hit )
	{
		return 0;
	}
	point->x = (float)r.point.x;
	point->y = (float)r.point.y;
	point->z = (float)r.point.z;
	*frac = r.fraction;
	return 1;
}

// Cast a sphere (radius 0 = a ray) at ONE body posed at an arbitrary
// transform — the lag-comp verb: the caller rewinds the pose, Box3D
// judges the geometry. Returns 0 on miss.
int pmb3_body_cast_sphere( uint64_t body, PmbVec3 tpos, PmbQuat trot, PmbVec3 origin, float radius,
						   PmbVec3 translation, PmbVec3* point, float* frac )
{
	b3QueryFilter filter = b3DefaultQueryFilter();
	filter.categoryBits = ~0ull;
	filter.maskBits = ~0ull;
	b3WorldTransform xf;
	xf.p = ( b3Pos ){ tpos.x, tpos.y, tpos.z };
	xf.q = ( b3Quat ){ .v = { trot.x, trot.y, trot.z }, .s = trot.w };
	b3BodyCastResult r;
	if ( radius > 0.0f )
	{
		b3Vec3 pt = { 0.0f, 0.0f, 0.0f };
		b3ShapeProxy proxy = { &pt, 1, radius };
		r = b3Body_CastShape( pmb3_unpack_body( body ), ( b3Pos ){ origin.x, origin.y, origin.z }, &proxy,
							  ( b3Vec3 ){ translation.x, translation.y, translation.z }, filter, 1.0f, false, xf );
	}
	else
	{
		r = b3Body_CastRay( pmb3_unpack_body( body ), ( b3Pos ){ origin.x, origin.y, origin.z },
							( b3Vec3 ){ translation.x, translation.y, translation.z }, filter, 1.0f, xf );
	}
	if ( !r.hit )
	{
		return 0;
	}
	point->x = (float)r.point.x;
	point->y = (float)r.point.y;
	point->z = (float)r.point.z;
	*frac = r.fraction;
	return 1;
}

// Overlap a capsule (p1..p2, radius) against the live world; bodies
// whose category is in `mask` land in `out` (deduped), up to `cap`.
typedef struct
{
	uint64_t* out;
	int cap;
	int n;
} PmbOverlapCtx;

static bool pmb3_overlap_cb( b3ShapeId shapeId, void* context )
{
	PmbOverlapCtx* ctx = (PmbOverlapCtx*)context;
	uint64_t body = pmb3_pack_body( b3Shape_GetBody( shapeId ) );
	for ( int i = 0; i < ctx->n; ++i )
	{
		if ( ctx->out[i] == body )
		{
			return true; // multi-shape body already collected
		}
	}
	ctx->out[ctx->n++] = body;
	return ctx->n < ctx->cap;
}

int pmb3_world_overlap_capsule( uint32_t w, PmbVec3 p1, PmbVec3 p2, float radius, uint64_t mask, uint64_t* out,
								int cap )
{
	b3QueryFilter filter = b3DefaultQueryFilter();
	filter.categoryBits = ~0ull;
	filter.maskBits = mask;
	b3Vec3 pts[2] = { { 0.0f, 0.0f, 0.0f }, { p2.x - p1.x, p2.y - p1.y, p2.z - p1.z } };
	b3ShapeProxy proxy = { pts, 2, radius };
	PmbOverlapCtx ctx = { out, cap, 0 };
	b3World_OverlapShape( pmb3_unpack_world( w ), ( b3Pos ){ p1.x, p1.y, p1.z }, &proxy, filter, pmb3_overlap_cb,
						  &ctx );
	return ctx.n;
}
